//! Bloom filters, one per row group per high-cardinality column.
//!
//! A zone map answers "could this row group contain a value in this *range*?".
//! It is excellent for a clustered column and useless for a scattered one:
//! every row group's min and max span the whole domain, so no range rules
//! anything out. A bloom filter answers a different question -- "could this row
//! group contain this *exact* value?" -- and it answers it well precisely when
//! the zone map cannot, because a scattered column is one where most values are
//! absent from most groups.
//!
//! The filter is one-sided: a negative is certain, a positive may be wrong. That
//! is the only shape a pruning structure may have. Saying "not here" when the
//! value is present would lose rows; saying "maybe" when it is absent only
//! costs a scan.
//!
//! Sizing follows the standard formulas for a target false-positive rate: about
//! 9.6 bits and 7 hashes per element at 1%. Both hashes come from splitting one
//! 64-bit hash rather than hashing twice -- Kirsch-Mitzenmacher double hashing,
//! which is indistinguishable in false-positive rate from independent hashes.

use crate::types::{hash_scalar, ScalarValue};

/// Target false-positive rate. 1% costs about 1.2 bytes per row and turns most
/// absent-value probes into a skipped row group; pushing it lower buys little
/// and costs memory linearly.
const FALSE_POSITIVE_RATE: f64 = 0.01;

#[derive(Debug, Clone)]
pub struct BloomFilter {
    words: Vec<u64>,
    num_bits: usize,
    num_hashes: u32,
}

impl BloomFilter {
    /// A filter sized for `expected` distinct values.
    pub fn with_capacity(expected: usize) -> BloomFilter {
        let expected = expected.max(1) as f64;
        // m = -n ln p / (ln 2)^2, k = (m/n) ln 2.
        let bits = (-expected * FALSE_POSITIVE_RATE.ln() / (std::f64::consts::LN_2.powi(2)))
            .ceil()
            .max(64.0) as usize;
        let hashes = ((bits as f64 / expected) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 16.0) as u32;
        BloomFilter {
            words: vec![0; bits.div_ceil(64)],
            num_bits: bits,
            num_hashes: hashes,
        }
    }

    pub fn insert(&mut self, value: &ScalarValue) {
        if value.is_null() {
            // A NULL is not a value, and `col = NULL` is never true, so nothing
            // would ever probe for one.
            return;
        }
        self.insert_hash(hash_scalar(value));
    }

    /// Insert a value by its hash, for callers reading a column directly rather
    /// than materializing a [`ScalarValue`] per row. The hash must come from
    /// `Column::hash_at`, which agrees with [`hash_scalar`] by construction.
    pub fn insert_hash(&mut self, hash: u64) {
        for bit in Self::bits(hash, self.num_bits, self.num_hashes) {
            self.words[bit / 64] |= 1u64 << (bit % 64);
        }
    }

    /// `false` means the value is definitely absent. `true` means it may be
    /// present -- which is all a filter can promise.
    pub fn might_contain(&self, value: &ScalarValue) -> bool {
        if value.is_null() {
            return false;
        }
        Self::bits(hash_scalar(value), self.num_bits, self.num_hashes)
            .all(|bit| self.words[bit / 64] & (1u64 << (bit % 64)) != 0)
    }

    /// The bit positions a hash maps to, by double hashing: `h1 + i * h2`. One
    /// 64-bit hash split in half behaves like two independent ones here.
    fn bits(hash: u64, num_bits: usize, num_hashes: u32) -> impl Iterator<Item = usize> {
        let h1 = (hash & 0xffff_ffff) as usize;
        // The second hash must be odd, or a multiple of the bit count would
        // make every probe land on the same bit.
        let h2 = ((hash >> 32) as usize) | 1;
        (0..num_hashes).map(move |i| h1.wrapping_add((i as usize).wrapping_mul(h2)) % num_bits)
    }

    pub fn byte_size(&self) -> usize {
        self.words.len() * 8
    }

    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    pub fn num_bits(&self) -> usize {
        self.num_bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(i: i64) -> ScalarValue {
        ScalarValue::Int64(i)
    }

    #[test]
    fn never_reports_a_false_negative() {
        // The one guarantee a bloom filter makes, and the only one pruning can
        // be built on.
        let mut f = BloomFilter::with_capacity(1000);
        for i in 0..1000 {
            f.insert(&value(i));
        }
        for i in 0..1000 {
            assert!(f.might_contain(&value(i)), "lost {i}");
        }
    }

    #[test]
    fn false_positives_stay_near_the_target_rate() {
        let mut f = BloomFilter::with_capacity(2000);
        for i in 0..2000 {
            f.insert(&value(i));
        }
        let positives = (10_000..20_000)
            .filter(|i| f.might_contain(&value(*i)))
            .count();
        let rate = positives as f64 / 10_000.0;
        assert!(rate < 0.03, "false-positive rate was {rate:.4}");
    }

    #[test]
    fn works_on_strings_and_floats() {
        let mut f = BloomFilter::with_capacity(500);
        for i in 0..500 {
            f.insert(&ScalarValue::Utf8(format!("value-{i}")));
            f.insert(&ScalarValue::Float64(i as f64 * 1.5));
        }
        assert!(f.might_contain(&ScalarValue::Utf8("value-250".into())));
        assert!(f.might_contain(&ScalarValue::Float64(375.0)));
        assert!(!f.might_contain(&ScalarValue::Utf8("absent".into())));
    }

    #[test]
    fn signed_zeros_hash_alike() {
        let mut f = BloomFilter::with_capacity(10);
        f.insert(&ScalarValue::Float64(0.0));
        assert!(f.might_contain(&ScalarValue::Float64(-0.0)));
    }

    #[test]
    fn nulls_are_neither_stored_nor_found() {
        let mut f = BloomFilter::with_capacity(10);
        f.insert(&ScalarValue::Null);
        assert!(!f.might_contain(&ScalarValue::Null));
    }

    #[test]
    fn an_empty_filter_finds_nothing() {
        let f = BloomFilter::with_capacity(100);
        assert!(!f.might_contain(&value(1)));
    }

    #[test]
    fn sizing_follows_the_target_rate() {
        // ~9.6 bits and ~7 hashes per element at 1%.
        let f = BloomFilter::with_capacity(10_000);
        let bits_per_element = f.num_bits() as f64 / 10_000.0;
        assert!(
            (bits_per_element - 9.6).abs() < 0.5,
            "{bits_per_element} bits per element"
        );
        assert_eq!(f.num_hashes(), 7);
    }
}
