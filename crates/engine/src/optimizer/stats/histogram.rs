//! Equi-depth histograms, for range selectivity.
//!
//! An equi-width histogram splits the *value range* into equal pieces, which is
//! useless on skewed data: if 99% of rows sit in one bucket, every range
//! estimate that touches it is a guess. An equi-depth histogram splits the
//! *rows* instead, so every bucket holds the same number of rows and the
//! boundaries land where the data actually is -- dense regions get many narrow
//! buckets, sparse regions get one wide one.
//!
//! Bucket `i` covers `(bounds[i], bounds[i + 1]]` and holds `1 / buckets` of the
//! non-NULL rows. Interpolating inside a bucket assumes values are spread evenly
//! within it, which is the assumption that makes a histogram cheap and also the
//! one that makes it wrong; more buckets narrows the range over which it has to
//! hold.

use std::cmp::Ordering;

use crate::types::{compare, ScalarValue};

/// How many buckets a histogram is built with. PostgreSQL's default statistics
/// target is 100; matching it makes the accuracy comparable to something whose
/// behaviour is well understood.
pub const DEFAULT_BUCKETS: usize = 100;

#[derive(Debug, Clone)]
pub struct Histogram {
    /// `buckets + 1` boundaries in ascending order.
    bounds: Vec<ScalarValue>,
}

impl Histogram {
    /// Build from non-NULL values. `values` is sorted in place.
    ///
    /// Returns `None` when there is not enough data for buckets to mean
    /// anything, in which case the caller falls back to min/max reasoning.
    pub fn build(values: &mut [ScalarValue], buckets: usize) -> Option<Histogram> {
        if values.len() < 2 || buckets == 0 {
            return None;
        }
        values.sort_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal));

        let buckets = buckets.min(values.len());
        let mut bounds = Vec::with_capacity(buckets + 1);
        for i in 0..=buckets {
            // Quantile i/buckets, clamped to the last element.
            let position = (i * (values.len() - 1)) / buckets;
            bounds.push(values[position].clone());
        }
        Some(Histogram { bounds })
    }

    pub fn buckets(&self) -> usize {
        self.bounds.len() - 1
    }

    pub fn min(&self) -> &ScalarValue {
        &self.bounds[0]
    }

    pub fn max(&self) -> &ScalarValue {
        &self.bounds[self.bounds.len() - 1]
    }

    /// Fraction of non-NULL rows whose value is `<= v`.
    pub fn fraction_at_most(&self, v: &ScalarValue) -> f64 {
        if compare(v, self.min()) == Some(Ordering::Less) {
            return 0.0;
        }
        if compare(v, self.max()) != Some(Ordering::Less) {
            return 1.0;
        }

        // Find the bucket containing v.
        let buckets = self.buckets();
        let mut index = 0;
        while index + 1 < buckets && compare(v, &self.bounds[index + 1]) != Some(Ordering::Less) {
            index += 1;
        }

        let within = interpolate(v, &self.bounds[index], &self.bounds[index + 1]);
        (index as f64 + within) / buckets as f64
    }

    /// Fraction of non-NULL rows in `[lo, hi]`.
    pub fn fraction_between(&self, lo: &ScalarValue, hi: &ScalarValue) -> f64 {
        if compare(lo, hi) == Some(Ordering::Greater) {
            return 0.0;
        }
        // `fraction_at_most(lo)` includes lo itself, which for a continuous
        // approximation is a rounding error not worth correcting.
        (self.fraction_at_most(hi) - self.fraction_at_most(lo)).clamp(0.0, 1.0)
    }
}

/// Where `v` sits between two bucket boundaries, as a fraction in `[0, 1]`.
///
/// Numeric and temporal values interpolate linearly. Strings have no usable
/// metric -- the distance between 'apple' and 'banana' is not a number -- so
/// they land in the middle of the bucket, which is the least-wrong constant.
fn interpolate(v: &ScalarValue, low: &ScalarValue, high: &ScalarValue) -> f64 {
    let (Some(v), Some(low), Some(high)) = (numeric(v), numeric(low), numeric(high)) else {
        return 0.5;
    };
    if high <= low {
        return 0.5;
    }
    ((v - low) / (high - low)).clamp(0.0, 1.0)
}

fn numeric(v: &ScalarValue) -> Option<f64> {
    match v {
        ScalarValue::Date32(d) => Some(*d as f64),
        ScalarValue::Timestamp(t) => Some(*t as f64),
        ScalarValue::Boolean(b) => Some(*b as i64 as f64),
        other => other.as_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(values: impl IntoIterator<Item = i64>) -> Vec<ScalarValue> {
        values.into_iter().map(ScalarValue::Int64).collect()
    }

    #[test]
    fn uniform_data_gives_proportional_estimates() {
        let mut values = ints(0..1000);
        let h = Histogram::build(&mut values, 100).unwrap();
        for point in [100i64, 250, 500, 750, 900] {
            let got = h.fraction_at_most(&ScalarValue::Int64(point));
            let want = point as f64 / 1000.0;
            assert!((got - want).abs() < 0.02, "at {point}: {got:.3} vs {want:.3}");
        }
    }

    #[test]
    fn skew_is_where_equi_depth_earns_its_keep() {
        // 90% of the rows are 0..99, the rest are spread to 100000. An
        // equi-width histogram would put nearly everything in one bucket.
        let mut values = ints(0..100);
        for _ in 0..8 {
            values.extend(ints(0..100));
        }
        values.extend(ints((0..100).map(|i| i * 1000)));
        let total = values.len() as f64;
        let below_100 = values
            .iter()
            .filter(|v| matches!(v, ScalarValue::Int64(i) if *i < 100))
            .count() as f64;

        let h = Histogram::build(&mut values, 100).unwrap();
        let got = h.fraction_at_most(&ScalarValue::Int64(99));
        assert!(
            (got - below_100 / total).abs() < 0.06,
            "estimated {got:.3}, actual {:.3}",
            below_100 / total
        );
    }

    #[test]
    fn out_of_range_values_saturate() {
        let mut values = ints(10..20);
        let h = Histogram::build(&mut values, 5).unwrap();
        assert_eq!(h.fraction_at_most(&ScalarValue::Int64(0)), 0.0);
        assert_eq!(h.fraction_at_most(&ScalarValue::Int64(100)), 1.0);
    }

    #[test]
    fn ranges_compose() {
        let mut values = ints(0..1000);
        let h = Histogram::build(&mut values, 100).unwrap();
        let got = h.fraction_between(&ScalarValue::Int64(200), &ScalarValue::Int64(400));
        assert!((got - 0.2).abs() < 0.03, "{got:.3}");
        // An inverted range selects nothing.
        assert_eq!(
            h.fraction_between(&ScalarValue::Int64(400), &ScalarValue::Int64(200)),
            0.0
        );
    }

    #[test]
    fn too_little_data_produces_no_histogram() {
        assert!(Histogram::build(&mut ints(0..1), 10).is_none());
        assert!(Histogram::build(&mut [], 10).is_none());
    }

    #[test]
    fn strings_fall_back_to_half_buckets() {
        let mut values: Vec<ScalarValue> = ('a'..='z')
            .map(|c| ScalarValue::Utf8(c.to_string()))
            .collect();
        let h = Histogram::build(&mut values, 26).unwrap();
        let got = h.fraction_at_most(&ScalarValue::Utf8("m".into()));
        assert!((got - 0.5).abs() < 0.1, "{got:.3}");
    }
}
