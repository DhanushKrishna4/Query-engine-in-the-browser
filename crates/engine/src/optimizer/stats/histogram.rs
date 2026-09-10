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
    /// How many values the buckets were built from.
    ///
    /// A histogram over a sample must not claim certainty about values the
    /// sample missed, and this is what bounds how wrong it is allowed to be
    /// about them. See `exact`.
    sample_size: usize,
    /// The column's true extremes, when they are known exactly.
    ///
    /// They usually are: zone maps carry min and max for *every* row group,
    /// computed from every row and costing nothing, while the buckets come
    /// from a sample of a few. So the histogram can know that values exist
    /// beyond its own outermost bucket even though it never saw one.
    exact: Option<(ScalarValue, ScalarValue)>,
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

        let sample_size = values.len();
        let buckets = buckets.min(values.len());
        let mut bounds = Vec::with_capacity(buckets + 1);
        for i in 0..=buckets {
            // Quantile i/buckets, clamped to the last element.
            let position = (i * (values.len() - 1)) / buckets;
            bounds.push(values[position].clone());
        }
        Some(Histogram { bounds, sample_size, exact: None })
    }

    /// Record the column's true extremes, from the zone maps.
    ///
    /// Only when they lie outside the sampled range, which is the case worth
    /// representing: a `max` the sample already reached tells the histogram
    /// nothing it does not know.
    pub fn with_exact_bounds(mut self, min: Option<&ScalarValue>, max: Option<&ScalarValue>) -> Histogram {
        let lo = match min {
            Some(v) if compare(v, self.min()) == Some(Ordering::Less) => v.clone(),
            _ => self.min().clone(),
        };
        let hi = match max {
            Some(v) if compare(v, self.max()) == Some(Ordering::Greater) => v.clone(),
            _ => self.max().clone(),
        };
        self.exact = Some((lo, hi));
        self
    }

    /// The mass the sample assigns to everything it did not see.
    ///
    /// One sample in `n` is the add-one estimate for an event that occurred
    /// zero times in `n` draws. It is a small number and the point is that it
    /// is not zero: a 20,000-row sample of seven million rows will not contain
    /// the rare tail, and a histogram that returns exactly 1.0 for
    /// `fraction_at_most(v)` is asserting that no row exceeds `v` -- which the
    /// zone maps can flatly contradict.
    fn tail_mass(&self) -> f64 {
        1.0 / (self.sample_size.max(1) as f64)
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
        // Outside the sampled range, but inside the column's real one: the
        // sample says nothing lives here and the zone maps say something does.
        // Split `tail_mass` across the gap rather than rounding to certainty.
        if let Some((lo, hi)) = &self.exact {
            if compare(v, self.min()) == Some(Ordering::Less) {
                if compare(v, lo) == Some(Ordering::Less) {
                    return 0.0;
                }
                return self.tail_mass() * position_in(v, lo, self.min());
            }
            if compare(v, self.max()) != Some(Ordering::Less) {
                if compare(v, hi) != Some(Ordering::Less) {
                    return 1.0;
                }
                let above = self.tail_mass() * (1.0 - position_in(v, self.max(), hi));
                return (1.0 - above).clamp(0.0, 1.0);
            }
        }
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

/// Where `v` sits between two values, as a fraction in `[0, 1]`.
///
/// The same interpolation the buckets use, applied to the gap between a
/// sampled extreme and the column's real one.
fn position_in(v: &ScalarValue, low: &ScalarValue, high: &ScalarValue) -> f64 {
    interpolate(v, low, high)
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

    /// A histogram over a sample must not declare its own tails empty.
    ///
    /// This is the shape of the bug it was written for: seven million taxi
    /// trips, a 20,000-row sample that reached about 30 miles, and
    /// `trip_distance > 60` estimated at zero rows because nothing in the
    /// sample went that far. The zone maps knew the longest trip was 830
    /// miles the whole time.
    #[test]
    fn a_sample_does_not_get_to_declare_the_tail_empty() {
        let mut values = ints(0..100);
        let sampled = Histogram::build(&mut values, 10).unwrap();
        let widened = sampled
            .clone()
            .with_exact_bounds(None, Some(&ScalarValue::Int64(1000)));

        // Without the real maximum, everything past the sample is impossible.
        assert_eq!(sampled.fraction_at_most(&ScalarValue::Int64(500)), 1.0);

        // With it, the tail is small and *not* zero, and it shrinks the
        // further out you ask.
        let near = 1.0 - widened.fraction_at_most(&ScalarValue::Int64(200));
        let far = 1.0 - widened.fraction_at_most(&ScalarValue::Int64(900));
        assert!(near > far, "{near} should exceed {far}");
        assert!(far > 0.0, "a value below the real maximum cannot be impossible");
        assert!(near < 0.02, "the tail is a sampling correction, not a guess: {near}");

        // Beyond the real maximum it is certain again, and below the real
        // minimum too.
        assert_eq!(widened.fraction_at_most(&ScalarValue::Int64(1000)), 1.0);
        assert_eq!(widened.fraction_at_most(&ScalarValue::Int64(1001)), 1.0);
        let widened = widened.with_exact_bounds(Some(&ScalarValue::Int64(-500)), None);
        assert_eq!(widened.fraction_at_most(&ScalarValue::Int64(-501)), 0.0);
        assert!(widened.fraction_at_most(&ScalarValue::Int64(-100)) > 0.0);
    }

    /// A sample that reached the real extremes learns nothing from being told
    /// what they are, and must not start hedging about them.
    #[test]
    fn exact_bounds_inside_the_sample_change_nothing() {
        let mut values = ints(0..100);
        let h = Histogram::build(&mut values, 10).unwrap();
        let told = h.clone().with_exact_bounds(
            Some(&ScalarValue::Int64(0)),
            Some(&ScalarValue::Int64(99)),
        );
        for point in [-1i64, 0, 25, 50, 99, 100] {
            let v = ScalarValue::Int64(point);
            assert_eq!(
                h.fraction_at_most(&v),
                told.fraction_at_most(&v),
                "at {point}"
            );
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
