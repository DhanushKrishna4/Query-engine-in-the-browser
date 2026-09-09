//! HyperLogLog: distinct-value counting in fixed memory.
//!
//! Counting distinct values exactly means keeping every value seen, which for a
//! high-cardinality column is the column again. HyperLogLog instead keeps one
//! small register per hash bucket, recording the longest run of leading zeros
//! seen in that bucket -- a long run is rare, so seeing one is evidence of many
//! distinct values. The harmonic mean across registers turns that evidence into
//! an estimate.
//!
//! `P = 12` gives 4096 registers, one byte each: 4 KiB per column, with a
//! standard error around 1.6% regardless of whether the column has a thousand
//! distinct values or a billion. That trade -- constant memory, bounded relative
//! error -- is why every query optimizer uses it.
//!
//! The estimator is the classic one with linear counting for small
//! cardinalities, where the raw estimate is badly biased. The large-range
//! correction from the original paper is for 32-bit hashes and is not needed
//! here.

use crate::types::ScalarValue;

const P: usize = 12;
const M: usize = 1 << P;

#[derive(Debug, Clone)]
pub struct HyperLogLog {
    registers: Vec<u8>,
}

impl Default for HyperLogLog {
    fn default() -> HyperLogLog {
        HyperLogLog::new()
    }
}

impl HyperLogLog {
    pub fn new() -> HyperLogLog {
        HyperLogLog {
            registers: vec![0; M],
        }
    }

    pub fn add(&mut self, value: &ScalarValue) {
        if value.is_null() {
            return;
        }
        self.add_hash(hash_scalar(value));
    }

    pub fn add_hash(&mut self, hash: u64) {
        // The top P bits choose the register; the rest supply the zero run.
        let index = (hash >> (64 - P)) as usize;
        let remaining = hash << P;
        let rank = if remaining == 0 {
            (64 - P + 1) as u8
        } else {
            (remaining.leading_zeros() + 1) as u8
        };
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }
    }

    pub fn estimate(&self) -> f64 {
        let m = M as f64;
        let harmonic: f64 = self
            .registers
            .iter()
            .map(|r| 2f64.powi(-(*r as i32)))
            .sum();
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let raw = alpha * m * m / harmonic;

        let empty = self.registers.iter().filter(|r| **r == 0).count();
        if raw <= 2.5 * m && empty > 0 {
            // Below roughly 2.5 registers' worth of values most registers are
            // still empty, and counting the empty ones is far more accurate.
            m * (m / empty as f64).ln()
        } else {
            raw
        }
    }

    pub fn merge(&mut self, other: &HyperLogLog) {
        for (a, b) in self.registers.iter_mut().zip(&other.registers) {
            *a = (*a).max(*b);
        }
    }
}

/// Hash a value for counting purposes.
///
/// Shared with the bloom filters, so a value that hashes one way for cardinality
/// estimation hashes the same way for membership.
pub use crate::types::hash_scalar;

#[cfg(test)]
mod tests {
    use super::*;

    fn estimate_of(distinct: usize) -> f64 {
        let mut hll = HyperLogLog::new();
        for i in 0..distinct {
            hll.add(&ScalarValue::Int64(i as i64));
        }
        hll.estimate()
    }

    #[test]
    fn is_accurate_across_five_orders_of_magnitude() {
        // The point of HLL is bounded *relative* error, so the tolerance is a
        // percentage rather than an absolute count.
        for n in [0usize, 1, 10, 100, 1_000, 10_000, 100_000] {
            let got = estimate_of(n);
            let tolerance = (n as f64 * 0.05).max(1.0);
            assert!(
                (got - n as f64).abs() <= tolerance,
                "n={n}: estimated {got:.1}, tolerance {tolerance:.1}"
            );
        }
    }

    #[test]
    fn repeats_do_not_inflate_the_estimate() {
        let mut hll = HyperLogLog::new();
        for _ in 0..1000 {
            for i in 0..50 {
                hll.add(&ScalarValue::Int64(i));
            }
        }
        assert!((hll.estimate() - 50.0).abs() < 5.0, "{}", hll.estimate());
    }

    #[test]
    fn nulls_are_not_values() {
        let mut hll = HyperLogLog::new();
        for _ in 0..100 {
            hll.add(&ScalarValue::Null);
        }
        assert_eq!(hll.estimate(), 0.0);
    }

    #[test]
    fn signed_zeros_count_once() {
        let mut hll = HyperLogLog::new();
        hll.add(&ScalarValue::Float64(0.0));
        hll.add(&ScalarValue::Float64(-0.0));
        assert!(hll.estimate() < 1.5, "{}", hll.estimate());
    }

    #[test]
    fn merging_unions_the_sets() {
        let mut a = HyperLogLog::new();
        let mut b = HyperLogLog::new();
        for i in 0..5000 {
            a.add(&ScalarValue::Int64(i));
        }
        for i in 2500..7500 {
            b.add(&ScalarValue::Int64(i));
        }
        a.merge(&b);
        // The union has 7500 distinct values, not 10000.
        assert!((a.estimate() - 7500.0).abs() < 400.0, "{}", a.estimate());
    }

    #[test]
    fn strings_work_too() {
        let mut hll = HyperLogLog::new();
        for i in 0..2000 {
            hll.add(&ScalarValue::Utf8(format!("value-{i}")));
        }
        assert!((hll.estimate() - 2000.0).abs() < 120.0, "{}", hll.estimate());
    }
}
