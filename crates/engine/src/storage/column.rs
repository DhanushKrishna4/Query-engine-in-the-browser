//! Columnar value storage.
//!
//! Every column is a contiguous typed buffer plus an optional validity bitmap.
//! NULL slots still occupy a position in the buffer (holding a placeholder
//! value) so that row `i` of every column in a row group is always at index `i`
//! -- no offset arithmetic, no branch, no per-value indirection.
//!
//! Strings use offset encoding: one `bytes` buffer holding every string
//! back-to-back, and an `offsets` array of `len + 1` entries. That is two
//! allocations for a whole column instead of one per value, and it keeps
//! scanning a string column sequential in memory.

use crate::error::{Diagnostic, Result};
use crate::storage::bitmap::Bitmap;
use crate::types::{DataType, ScalarValue};

/// Offset-encoded UTF-8 strings. `offsets[i]..offsets[i + 1]` is the byte range
/// of value `i` inside `bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringColumn {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl StringColumn {
    pub fn new() -> StringColumn {
        StringColumn {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    pub fn with_capacity(values: usize, bytes: usize) -> StringColumn {
        let mut offsets = Vec::with_capacity(values + 1);
        offsets.push(0);
        StringColumn {
            offsets,
            bytes: Vec::with_capacity(bytes),
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn push(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
        self.offsets.push(self.bytes.len() as u32);
    }

    #[inline]
    pub fn get(&self, i: usize) -> &str {
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        // Safe by construction: every byte in `bytes` came from a `&str`, and
        // offsets are only ever appended at value boundaries.
        std::str::from_utf8(&self.bytes[start..end]).expect("string column holds valid UTF-8")
    }

    pub fn total_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).map(|i| self.get(i))
    }
}

impl Default for StringColumn {
    fn default() -> Self {
        StringColumn::new()
    }
}

/// The physical representation of each logical type.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    /// Every value is NULL; carries only a length. Produced by binding a bare
    /// `NULL` literal before it has been coerced to a concrete type.
    Null(usize),
    Boolean(Bitmap),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Utf8(StringColumn),
    Date32(Vec<i32>),
    Timestamp(Vec<i64>),
    Decimal128 {
        values: Vec<i128>,
        precision: u8,
        scale: i8,
    },
}

impl ColumnData {
    pub fn len(&self) -> usize {
        match self {
            ColumnData::Null(n) => *n,
            ColumnData::Boolean(b) => b.len(),
            ColumnData::Int32(v) => v.len(),
            ColumnData::Int64(v) => v.len(),
            ColumnData::Float64(v) => v.len(),
            ColumnData::Utf8(s) => s.len(),
            ColumnData::Date32(v) => v.len(),
            ColumnData::Timestamp(v) => v.len(),
            ColumnData::Decimal128 { values, .. } => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        match self {
            ColumnData::Null(_) => DataType::Null,
            ColumnData::Boolean(_) => DataType::Boolean,
            ColumnData::Int32(_) => DataType::Int32,
            ColumnData::Int64(_) => DataType::Int64,
            ColumnData::Float64(_) => DataType::Float64,
            ColumnData::Utf8(_) => DataType::Utf8,
            ColumnData::Date32(_) => DataType::Date32,
            ColumnData::Timestamp(_) => DataType::Timestamp,
            ColumnData::Decimal128 { precision, scale, .. } => DataType::Decimal128 {
                precision: *precision,
                scale: *scale,
            },
        }
    }

    /// Bytes held by the value buffer, ignoring the validity bitmap. Used by
    /// the storage inspector to report compression ratios later on.
    pub fn byte_size(&self) -> usize {
        match self {
            ColumnData::Null(_) => 0,
            ColumnData::Boolean(b) => b.len().div_ceil(8),
            ColumnData::Int32(v) => v.len() * 4,
            ColumnData::Int64(v) => v.len() * 8,
            ColumnData::Float64(v) => v.len() * 8,
            ColumnData::Utf8(s) => s.total_bytes() + (s.len() + 1) * 4,
            ColumnData::Date32(v) => v.len() * 4,
            ColumnData::Timestamp(v) => v.len() * 8,
            ColumnData::Decimal128 { values, .. } => values.len() * 16,
        }
    }
}

/// A column: values plus the mask that says which of them are real.
///
/// `validity == None` means "no NULLs in this column", which is both a memory
/// saving and a fast path: predicate evaluation can skip the per-row validity
/// check entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub data: ColumnData,
    pub validity: Option<Bitmap>,
}

impl Column {
    pub fn new(data: ColumnData, validity: Option<Bitmap>) -> Column {
        if let Some(v) = &validity {
            debug_assert_eq!(v.len(), data.len());
        }
        Column { data, validity }
    }

    /// A column with no NULLs.
    pub fn dense(data: ColumnData) -> Column {
        Column { data, validity: None }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        match &self.validity {
            None => !matches!(self.data, ColumnData::Null(_)),
            Some(v) => v.get(i),
        }
    }

    pub fn null_count(&self) -> usize {
        match &self.validity {
            None => match self.data {
                ColumnData::Null(n) => n,
                _ => 0,
            },
            Some(v) => v.count_unset(),
        }
    }

    /// Materialize row `i` as a scalar. This is the interface the reference
    /// row-at-a-time evaluator uses; the vectorized kernels that replace it
    /// will read the buffers directly instead.
    pub fn value(&self, i: usize) -> ScalarValue {
        if !self.is_valid(i) {
            return ScalarValue::Null;
        }
        match &self.data {
            ColumnData::Null(_) => ScalarValue::Null,
            ColumnData::Boolean(b) => ScalarValue::Boolean(b.get(i)),
            ColumnData::Int32(v) => ScalarValue::Int32(v[i]),
            ColumnData::Int64(v) => ScalarValue::Int64(v[i]),
            ColumnData::Float64(v) => ScalarValue::Float64(v[i]),
            ColumnData::Utf8(s) => ScalarValue::Utf8(s.get(i).to_string()),
            ColumnData::Date32(v) => ScalarValue::Date32(v[i]),
            ColumnData::Timestamp(v) => ScalarValue::Timestamp(v[i]),
            ColumnData::Decimal128 { values, precision, scale } => ScalarValue::Decimal128 {
                value: values[i],
                precision: *precision,
                scale: *scale,
            },
        }
    }

    /// Contiguous sub-range. Copies today; once batches carry selection
    /// vectors and shared buffers this becomes a view.
    pub fn slice(&self, offset: usize, len: usize) -> Column {
        debug_assert!(offset + len <= self.len());
        let data = match &self.data {
            ColumnData::Null(_) => ColumnData::Null(len),
            ColumnData::Boolean(b) => ColumnData::Boolean(b.slice(offset, len)),
            ColumnData::Int32(v) => ColumnData::Int32(v[offset..offset + len].to_vec()),
            ColumnData::Int64(v) => ColumnData::Int64(v[offset..offset + len].to_vec()),
            ColumnData::Float64(v) => ColumnData::Float64(v[offset..offset + len].to_vec()),
            ColumnData::Utf8(s) => {
                let mut out = StringColumn::with_capacity(len, 0);
                for i in offset..offset + len {
                    out.push(s.get(i));
                }
                ColumnData::Utf8(out)
            }
            ColumnData::Date32(v) => ColumnData::Date32(v[offset..offset + len].to_vec()),
            ColumnData::Timestamp(v) => ColumnData::Timestamp(v[offset..offset + len].to_vec()),
            ColumnData::Decimal128 { values, precision, scale } => ColumnData::Decimal128 {
                values: values[offset..offset + len].to_vec(),
                precision: *precision,
                scale: *scale,
            },
        };
        Column {
            data,
            validity: self.validity.as_ref().map(|v| v.slice(offset, len)),
        }
    }

    /// Gather the rows named by `indices`. This is how a filter materializes
    /// its surviving rows today; the selection-vector design will make it
    /// conditional on selectivity.
    pub fn take(&self, indices: &[usize]) -> Column {
        let data = match &self.data {
            ColumnData::Null(_) => ColumnData::Null(indices.len()),
            ColumnData::Boolean(b) => ColumnData::Boolean(b.take(indices)),
            ColumnData::Int32(v) => ColumnData::Int32(indices.iter().map(|&i| v[i]).collect()),
            ColumnData::Int64(v) => ColumnData::Int64(indices.iter().map(|&i| v[i]).collect()),
            ColumnData::Float64(v) => ColumnData::Float64(indices.iter().map(|&i| v[i]).collect()),
            ColumnData::Utf8(s) => {
                let mut out = StringColumn::with_capacity(indices.len(), 0);
                for &i in indices {
                    out.push(s.get(i));
                }
                ColumnData::Utf8(out)
            }
            ColumnData::Date32(v) => ColumnData::Date32(indices.iter().map(|&i| v[i]).collect()),
            ColumnData::Timestamp(v) => {
                ColumnData::Timestamp(indices.iter().map(|&i| v[i]).collect())
            }
            ColumnData::Decimal128 { values, precision, scale } => ColumnData::Decimal128 {
                values: indices.iter().map(|&i| values[i]).collect(),
                precision: *precision,
                scale: *scale,
            },
        };
        Column {
            data,
            validity: self.validity.as_ref().map(|v| v.take(indices)),
        }
    }

    /// Gather rows by index, where `None` produces a NULL.
    ///
    /// This is how outer joins pad: an unmatched row asks for every column of
    /// the other side and gets NULLs back, without the join operator needing to
    /// know anything about the column's type.
    ///
    /// Typed rather than going through `ScalarValue`, because a join gathers
    /// once per output row and routing a string column through `ScalarValue`
    /// would allocate a `String` for every value it moves.
    pub fn take_opt(&self, indices: &[Option<usize>]) -> Column {
        // An inner join never pads, so the common case is a plain gather.
        if indices.iter().all(Option::is_some) {
            let dense: Vec<usize> = indices.iter().map(|i| i.expect("checked")).collect();
            return self.take(&dense);
        }

        macro_rules! gather {
            ($vals:expr, $variant:path) => {{
                let mut out = Vec::with_capacity(indices.len());
                for i in indices {
                    out.push(match i {
                        Some(i) => $vals[*i],
                        None => Default::default(),
                    });
                }
                $variant(out)
            }};
        }

        let data = match &self.data {
            ColumnData::Null(_) => ColumnData::Null(indices.len()),
            ColumnData::Int32(v) => gather!(v, ColumnData::Int32),
            ColumnData::Int64(v) => gather!(v, ColumnData::Int64),
            ColumnData::Float64(v) => gather!(v, ColumnData::Float64),
            ColumnData::Date32(v) => gather!(v, ColumnData::Date32),
            ColumnData::Timestamp(v) => gather!(v, ColumnData::Timestamp),
            ColumnData::Boolean(b) => ColumnData::Boolean(
                indices
                    .iter()
                    .map(|i| i.is_some_and(|i| b.get(i)))
                    .collect(),
            ),
            ColumnData::Utf8(s) => {
                let mut out = StringColumn::with_capacity(indices.len(), 0);
                for i in indices {
                    out.push(match i {
                        Some(i) => s.get(*i),
                        None => "",
                    });
                }
                ColumnData::Utf8(out)
            }
            ColumnData::Decimal128 { values, precision, scale } => {
                let mut out = Vec::with_capacity(indices.len());
                for i in indices {
                    out.push(i.map_or(0, |i| values[i]));
                }
                ColumnData::Decimal128 {
                    values: out,
                    precision: *precision,
                    scale: *scale,
                }
            }
        };

        // A padded slot is NULL, and so is a slot that was NULL to begin with.
        let validity: Bitmap = indices
            .iter()
            .map(|i| i.is_some_and(|i| self.is_valid(i)))
            .collect();
        Column::new(data, Some(validity))
    }

    /// Smallest and largest non-NULL values. This is the zone map: with it, a
    /// predicate like `x > 1000` can rule out a whole row group without reading
    /// a single value. Returns `None` when every value is NULL.
    pub fn min_max(&self) -> (Option<ScalarValue>, Option<ScalarValue>) {
        let mut min: Option<ScalarValue> = None;
        let mut max: Option<ScalarValue> = None;
        for i in 0..self.len() {
            if !self.is_valid(i) {
                continue;
            }
            let v = self.value(i);
            match &min {
                None => min = Some(v.clone()),
                Some(cur) => {
                    if crate::types::compare(&v, cur) == Some(std::cmp::Ordering::Less) {
                        min = Some(v.clone());
                    }
                }
            }
            match &max {
                None => max = Some(v),
                Some(cur) => {
                    if crate::types::compare(&v, cur) == Some(std::cmp::Ordering::Greater) {
                        max = Some(v);
                    }
                }
            }
        }
        (min, max)
    }

    pub fn byte_size(&self) -> usize {
        self.data.byte_size() + self.validity.as_ref().map_or(0, |v| v.len().div_ceil(8))
    }

    /// Hash the value at `i`, or `None` if it is NULL.
    ///
    /// This must agree exactly with `types::hash_scalar` on the equivalent
    /// `ScalarValue`, because the bloom filters are *built* from columns and
    /// *probed* with literals. A mismatch would not fail loudly -- it would
    /// silently prune row groups that do contain the value.
    pub fn hash_at(&self, i: usize) -> Option<u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        if !self.is_valid(i) {
            return None;
        }
        let mut h = DefaultHasher::new();
        match &self.data {
            ColumnData::Null(_) => return None,
            ColumnData::Boolean(b) => b.get(i).hash(&mut h),
            // Widened to i64 so an Int32 column and an Int64 literal of the
            // same value agree.
            ColumnData::Int32(v) => (v[i] as i64).hash(&mut h),
            ColumnData::Int64(v) => v[i].hash(&mut h),
            ColumnData::Date32(v) => (v[i] as i64).hash(&mut h),
            ColumnData::Timestamp(v) => v[i].hash(&mut h),
            ColumnData::Float64(v) => {
                let f = v[i];
                let bits = if f == 0.0 {
                    0.0f64.to_bits()
                } else if f.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    f.to_bits()
                };
                bits.hash(&mut h);
            }
            ColumnData::Utf8(s) => s.get(i).hash(&mut h),
            ColumnData::Decimal128 { values, scale, .. } => {
                crate::types::rescale(values[i], *scale, 18)
                    .unwrap_or(values[i])
                    .hash(&mut h);
            }
        }
        Some(h.finish())
    }
}

/// Accumulates scalars into a `Column`. Used by the CSV loader and by every
/// operator that produces new values (Project, and later the aggregates).
pub struct ColumnBuilder {
    data: ColumnData,
    validity: Bitmap,
    saw_null: bool,
}

impl ColumnBuilder {
    pub fn new(dt: &DataType) -> ColumnBuilder {
        let data = match dt {
            DataType::Null => ColumnData::Null(0),
            DataType::Boolean => ColumnData::Boolean(Bitmap::new()),
            DataType::Int32 => ColumnData::Int32(Vec::new()),
            DataType::Int64 => ColumnData::Int64(Vec::new()),
            DataType::Float64 => ColumnData::Float64(Vec::new()),
            DataType::Utf8 => ColumnData::Utf8(StringColumn::new()),
            DataType::Date32 => ColumnData::Date32(Vec::new()),
            DataType::Timestamp => ColumnData::Timestamp(Vec::new()),
            DataType::Decimal128 { precision, scale } => ColumnData::Decimal128 {
                values: Vec::new(),
                precision: *precision,
                scale: *scale,
            },
        };
        ColumnBuilder {
            data,
            validity: Bitmap::new(),
            saw_null: false,
        }
    }

    pub fn len(&self) -> usize {
        self.validity.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn append_null(&mut self) {
        self.saw_null = true;
        self.validity.push(false);
        // A NULL still occupies a slot so that row indices line up across
        // columns; the placeholder value is never observable.
        let placeholder = ScalarValue::null_placeholder(&self.data.data_type());
        self.push_value(&placeholder)
            .expect("placeholder always matches the builder type");
    }

    /// Append a value, which must already have been coerced to this builder's
    /// type by the binder. A mismatch is an engine bug, not a user error, so it
    /// surfaces as an execution diagnostic rather than being silently coerced.
    pub fn append(&mut self, v: &ScalarValue) -> Result<()> {
        if v.is_null() {
            self.append_null();
            return Ok(());
        }
        self.validity.push(true);
        self.push_value(v)
    }

    fn push_value(&mut self, v: &ScalarValue) -> Result<()> {
        let target = self.data.data_type();
        let mismatch = || {
            Diagnostic::exec(format!(
                "internal: cannot append a {} value to a {target} column",
                v.data_type(),
            ))
        };
        match (&mut self.data, v) {
            (ColumnData::Null(n), _) => *n += 1,
            (ColumnData::Boolean(b), ScalarValue::Boolean(x)) => b.push(*x),
            (ColumnData::Int32(dst), ScalarValue::Int32(x)) => dst.push(*x),
            (ColumnData::Int64(dst), ScalarValue::Int64(x)) => dst.push(*x),
            (ColumnData::Int64(dst), ScalarValue::Int32(x)) => dst.push(*x as i64),
            (ColumnData::Float64(dst), ScalarValue::Float64(x)) => dst.push(*x),
            (ColumnData::Utf8(dst), ScalarValue::Utf8(x)) => dst.push(x),
            (ColumnData::Date32(dst), ScalarValue::Date32(x)) => dst.push(*x),
            (ColumnData::Timestamp(dst), ScalarValue::Timestamp(x)) => dst.push(*x),
            (
                ColumnData::Decimal128 { values, scale, .. },
                ScalarValue::Decimal128 { value, scale: from, .. },
            ) => {
                let v = crate::types::rescale(*value, *from, *scale).ok_or_else(mismatch)?;
                values.push(v);
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }

    pub fn finish(self) -> Column {
        Column {
            data: self.data,
            // Dropping an all-valid bitmap is what makes the no-NULL fast path
            // available downstream.
            validity: if self.saw_null { Some(self.validity) } else { None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_column_is_offset_encoded() {
        let mut s = StringColumn::new();
        for v in ["", "a", "hello", "héllo"] {
            s.push(v);
        }
        assert_eq!(s.len(), 4);
        assert_eq!(s.get(0), "");
        assert_eq!(s.get(2), "hello");
        assert_eq!(s.get(3), "héllo");
        // One contiguous byte buffer, not four allocations.
        assert_eq!(s.total_bytes(), "ahellohéllo".len());
    }

    #[test]
    fn nulls_keep_their_slot_so_row_indices_stay_aligned() {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        b.append(&ScalarValue::Int64(1)).unwrap();
        b.append_null();
        b.append(&ScalarValue::Int64(3)).unwrap();
        let c = b.finish();

        assert_eq!(c.len(), 3);
        assert_eq!(c.null_count(), 1);
        assert!(!c.is_valid(1));
        assert_eq!(c.value(0), ScalarValue::Int64(1));
        assert!(c.value(1).is_null());
        assert_eq!(c.value(2), ScalarValue::Int64(3));
    }

    #[test]
    fn column_without_nulls_drops_the_bitmap() {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        b.append(&ScalarValue::Int64(1)).unwrap();
        assert!(b.finish().validity.is_none());
    }

    #[test]
    fn zone_map_ignores_nulls() {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        b.append_null();
        b.append(&ScalarValue::Int64(7)).unwrap();
        b.append(&ScalarValue::Int64(-2)).unwrap();
        let (min, max) = b.finish().min_max();
        assert_eq!(min, Some(ScalarValue::Int64(-2)));
        assert_eq!(max, Some(ScalarValue::Int64(7)));
    }

    #[test]
    fn all_null_column_has_no_zone_map() {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        b.append_null();
        let (min, max) = b.finish().min_max();
        assert!(min.is_none() && max.is_none());
    }

    #[test]
    fn take_opt_pads_with_nulls() {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        for v in [1i64, 2, 3] {
            b.append(&ScalarValue::Int64(v)).unwrap();
        }
        let c = b.finish().take_opt(&[Some(2), None, Some(0)]);
        assert_eq!(c.len(), 3);
        assert_eq!(c.value(0), ScalarValue::Int64(3));
        assert!(c.value(1).is_null());
        assert_eq!(c.value(2), ScalarValue::Int64(1));
    }

    #[test]
    fn slice_and_take_carry_validity() {
        let mut b = ColumnBuilder::new(&DataType::Utf8);
        for v in ["a", "b", "c", "d"] {
            b.append(&ScalarValue::Utf8(v.into())).unwrap();
        }
        b.append_null();
        let c = b.finish();

        let s = c.slice(3, 2);
        assert_eq!(s.value(0), ScalarValue::Utf8("d".into()));
        assert!(s.value(1).is_null());

        let t = c.take(&[4, 0]);
        assert!(t.value(0).is_null());
        assert_eq!(t.value(1), ScalarValue::Utf8("a".into()));
    }
}
