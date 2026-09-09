//! Bit-packed boolean vector, used for validity (NULL) masks and for the
//! physical representation of BOOLEAN columns.
//!
//! One bit per row, LSB-first within each u64 word. A column with no NULLs
//! stores `None` instead of an all-ones bitmap, so the common case costs
//! nothing to carry and nothing to check.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    pub fn new() -> Bitmap {
        Bitmap { words: Vec::new(), len: 0 }
    }

    pub fn with_capacity(cap: usize) -> Bitmap {
        Bitmap {
            words: Vec::with_capacity(cap.div_ceil(64)),
            len: 0,
        }
    }

    pub fn all_set(len: usize) -> Bitmap {
        let mut b = Bitmap {
            words: vec![u64::MAX; len.div_ceil(64)],
            len,
        };
        b.clear_trailing_bits();
        b
    }

    pub fn all_unset(len: usize) -> Bitmap {
        Bitmap {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    #[inline]
    pub fn set(&mut self, i: usize, value: bool) {
        debug_assert!(i < self.len);
        let (w, bit) = (i / 64, i % 64);
        if value {
            self.words[w] |= 1u64 << bit;
        } else {
            self.words[w] &= !(1u64 << bit);
        }
    }

    #[inline]
    pub fn push(&mut self, value: bool) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        self.len += 1;
        self.set(self.len - 1, value);
    }

    /// Number of set bits. `count_ones` over whole words is why the trailing
    /// bits past `len` must always be kept zeroed.
    pub fn count_set(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn count_unset(&self) -> usize {
        self.len - self.count_set()
    }

    pub fn slice(&self, offset: usize, len: usize) -> Bitmap {
        debug_assert!(offset + len <= self.len);
        let mut out = Bitmap::with_capacity(len);
        for i in 0..len {
            out.push(self.get(offset + i));
        }
        out
    }

    pub fn take(&self, indices: &[usize]) -> Bitmap {
        let mut out = Bitmap::with_capacity(indices.len());
        for &i in indices {
            out.push(self.get(i));
        }
        out
    }

    /// The raw backing words, LSB-first. Exposed so that boolean kernels can
    /// work 64 bits at a time instead of one bit at a time.
    #[inline]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn from_words(words: Vec<u64>, len: usize) -> Bitmap {
        debug_assert!(words.len() >= len.div_ceil(64));
        let mut b = Bitmap { words, len };
        b.clear_trailing_bits();
        b
    }

    /// Bitwise AND. Used to combine validity masks: a result is valid only
    /// where every input was.
    pub fn and(&self, other: &Bitmap) -> Bitmap {
        debug_assert_eq!(self.len, other.len);
        Bitmap {
            words: self
                .words
                .iter()
                .zip(&other.words)
                .map(|(a, b)| a & b)
                .collect(),
            len: self.len,
        }
    }

    pub fn or(&self, other: &Bitmap) -> Bitmap {
        debug_assert_eq!(self.len, other.len);
        Bitmap {
            words: self
                .words
                .iter()
                .zip(&other.words)
                .map(|(a, b)| a | b)
                .collect(),
            len: self.len,
        }
    }

    pub fn not(&self) -> Bitmap {
        let mut out = Bitmap {
            words: self.words.iter().map(|w| !w).collect(),
            len: self.len,
        };
        out.clear_trailing_bits();
        out
    }

    /// Indices of the set bits, in order.
    ///
    /// Scans a word at a time and pops the lowest set bit with `trailing_zeros`,
    /// so a sparse bitmap costs one iteration per set bit rather than one per
    /// row. This is how a filter turns a predicate result into a selection.
    pub fn set_indices(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.count_set());
        for (w, mut word) in self.words.iter().copied().enumerate() {
            let base = (w * 64) as u32;
            while word != 0 {
                out.push(base + word.trailing_zeros());
                word &= word - 1;
            }
        }
        out
    }

    /// Zero out the bits between `len` and the end of the final word, so that
    /// `count_set` stays correct after `all_set`.
    fn clear_trailing_bits(&mut self) {
        let rem = self.len % 64;
        if rem != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
    }
}

impl Default for Bitmap {
    fn default() -> Self {
        Bitmap::new()
    }
}

impl FromIterator<bool> for Bitmap {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Bitmap {
        let mut b = Bitmap::new();
        for v in iter {
            b.push(v);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_get_set_roundtrip() {
        let pattern: Vec<bool> = (0..200).map(|i| i % 3 == 0).collect();
        let b: Bitmap = pattern.iter().copied().collect();
        assert_eq!(b.len(), 200);
        for (i, &want) in pattern.iter().enumerate() {
            assert_eq!(b.get(i), want, "bit {i}");
        }
        assert_eq!(b.count_set(), pattern.iter().filter(|v| **v).count());
    }

    #[test]
    fn all_set_zeroes_the_tail() {
        // 100 bits spans two words; the 28 unused bits must not be counted.
        let b = Bitmap::all_set(100);
        assert_eq!(b.count_set(), 100);
        assert_eq!(Bitmap::all_unset(100).count_set(), 0);
    }

    #[test]
    fn logical_ops_respect_the_tail() {
        let a = Bitmap::all_set(100);
        let b = Bitmap::all_unset(100);
        assert_eq!(a.and(&b).count_set(), 0);
        assert_eq!(a.or(&b).count_set(), 100);
        // `not` must re-clear the bits past `len`, or count_set overcounts.
        assert_eq!(b.not().count_set(), 100);
        assert_eq!(a.not().count_set(), 0);
    }

    #[test]
    fn set_indices_finds_every_bit() {
        let pattern: Vec<bool> = (0..200).map(|i| i % 7 == 3).collect();
        let b: Bitmap = pattern.iter().copied().collect();
        let want: Vec<u32> = pattern
            .iter()
            .enumerate()
            .filter(|(_, v)| **v)
            .map(|(i, _)| i as u32)
            .collect();
        assert_eq!(b.set_indices(), want);
        assert!(Bitmap::all_unset(130).set_indices().is_empty());
    }

    #[test]
    fn slice_and_take() {
        let b: Bitmap = (0..70).map(|i| i % 2 == 0).collect();
        let s = b.slice(64, 6);
        assert_eq!(s.len(), 6);
        assert!(s.get(0)); // index 64 is even
        assert!(!s.get(1));

        let t = b.take(&[1, 2, 69]);
        assert_eq!(t.len(), 3);
        assert!(!t.get(0));
        assert!(t.get(1));
        assert!(!t.get(2));
    }
}
