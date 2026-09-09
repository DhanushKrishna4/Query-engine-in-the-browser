//! The unit of data that flows between operators: a set of columns plus a
//! selection saying which of their rows are live.
//!
//! Two ideas do most of the work here.
//!
//! **Batches are views, not copies.** Columns are held behind `Arc`, so a scan
//! hands out a window onto a row group without touching a byte of data. Only an
//! operator that produces genuinely new values (Project) allocates.
//!
//! **Filters produce a selection, not a new batch.** A filter that keeps 40% of
//! rows would otherwise rewrite every column to keep 40% of them -- including
//! columns nothing downstream reads. Instead it records *which* rows survived
//! and lets later operators read through that. The indirection costs one extra
//! load per access; the copy costs a full pass over every column. Indirection
//! wins until the surviving fraction gets small, at which point `compact` earns
//! its keep -- see `Selection::should_compact`.
//!
//! `DEFAULT_BATCH_SIZE` is 2048: large enough that per-batch call overhead
//! disappears, small enough that a batch's working set stays in L1/L2 rather
//! than streaming through cache.

use std::sync::Arc;

use crate::storage::column::Column;
use crate::storage::schema::Schema;

pub const DEFAULT_BATCH_SIZE: usize = 2048;

/// Which physical rows of a batch's columns are live, and in what order.
#[derive(Debug, Clone)]
pub enum Selection {
    /// The contiguous physical range `[offset, offset + len)`. What a scan
    /// produces, and the shape every kernel has a sequential fast path for.
    Range { offset: usize, len: usize },
    /// Arbitrary physical row indices, in order. What a filter produces.
    Indices(Arc<Vec<u32>>),
}

impl Selection {
    pub fn full(len: usize) -> Selection {
        Selection::Range { offset: 0, len }
    }

    pub fn len(&self) -> usize {
        match self {
            Selection::Range { len, .. } => *len,
            Selection::Indices(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this selection is a contiguous run, which lets kernels read a
    /// slice directly instead of gathering.
    pub fn is_contiguous(&self) -> bool {
        matches!(self, Selection::Range { .. })
    }

    #[inline]
    pub fn physical(&self, logical: usize) -> usize {
        match self {
            Selection::Range { offset, .. } => offset + logical,
            Selection::Indices(v) => v[logical] as usize,
        }
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = usize> + '_> {
        match self {
            Selection::Range { offset, len } => Box::new(*offset..*offset + *len),
            Selection::Indices(v) => Box::new(v.iter().map(|i| *i as usize)),
        }
    }

    /// Narrow this selection to the logical positions named by `keep`.
    ///
    /// This composition is what lets a filter stack on a filter: `keep` indexes
    /// into *this* selection's logical positions, and the result indexes into
    /// the same physical columns.
    pub fn select(&self, keep: &[u32]) -> Selection {
        if keep.len() == self.len() {
            // Nothing was filtered out.
            return self.clone();
        }
        match self {
            Selection::Range { offset, .. } => {
                Selection::Indices(Arc::new(keep.iter().map(|k| *k + *offset as u32).collect()))
            }
            Selection::Indices(v) => {
                Selection::Indices(Arc::new(keep.iter().map(|k| v[*k as usize]).collect()))
            }
        }
    }

    /// Whether reading through this selection has become more expensive than
    /// copying the surviving rows into a dense batch.
    ///
    /// The trade: indirection costs one extra load per value read, forever, for
    /// every downstream operator. Compaction costs one pass over every column
    /// once. So compaction wins when few rows survive out of a wide physical
    /// range -- both because the copy is then small and because the gather it
    /// removes was scattered across a range too large to stay in cache.
    ///
    /// `window` is how many physical rows this batch *spans*, not how long the
    /// underlying columns are. Those differ: a batch is a 2048-row window onto
    /// a 65536-row row group, and it is the window that bounds how far the
    /// gather scatters. Using the column length here made every filter look
    /// sparse and compacted batches that were 94% full.
    pub fn should_compact(&self, window: usize, threshold: f64) -> bool {
        if self.is_contiguous() || window == 0 {
            return false;
        }
        (self.len() as f64) < threshold * window as f64
    }
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub schema: Arc<Schema>,
    /// Physical columns, all of the same length, shared rather than copied.
    columns: Vec<Arc<Column>>,
    selection: Selection,
    /// Length of the underlying columns. Kept explicitly so a batch with no
    /// columns (the input to a FROM-less SELECT) still has a length.
    column_len: usize,
    /// How many physical rows this batch spans -- its width before any filter
    /// narrowed it. Preserved across filtering, because it is what bounds how
    /// far a gather has to reach.
    window: usize,
}

impl Batch {
    pub fn new(schema: Arc<Schema>, columns: Vec<Arc<Column>>, selection: Selection) -> Batch {
        debug_assert_eq!(schema.len(), columns.len());
        let column_len = columns.first().map_or(selection.len(), |c| c.len());
        debug_assert!(columns.iter().all(|c| c.len() == column_len));
        let window = selection.len();
        Batch {
            schema,
            columns,
            selection,
            column_len,
            window,
        }
    }

    /// A dense batch: every row of every column is live, in order.
    ///
    /// The row count is inferred from the first column, so this cannot express
    /// a batch with no columns -- use `new` with an explicit selection for
    /// those. They are reachable: projection pushdown can prune every column
    /// from a scan under `SELECT COUNT(*)`.
    pub fn dense(schema: Arc<Schema>, columns: Vec<Column>) -> Batch {
        let len = columns.first().map_or(0, |c| c.len());
        Batch::new(
            schema,
            columns.into_iter().map(Arc::new).collect(),
            Selection::full(len),
        )
    }

    /// A batch with rows but no columns. This is what a FROM-less `SELECT 1`
    /// scans: one row of nothing, so the projection evaluates exactly once.
    pub fn empty_rows(schema: Arc<Schema>, num_rows: usize) -> Batch {
        Batch {
            schema,
            columns: Vec::new(),
            selection: Selection::full(num_rows),
            column_len: num_rows,
            window: num_rows,
        }
    }

    /// Number of live rows.
    #[inline]
    pub fn num_rows(&self) -> usize {
        self.selection.len()
    }

    pub fn is_empty(&self) -> bool {
        self.num_rows() == 0
    }

    /// Length of the underlying columns, which is >= `num_rows`.
    #[inline]
    pub fn column_len(&self) -> usize {
        self.column_len
    }

    /// Physical rows this batch spans before filtering. The denominator for
    /// the compaction decision.
    #[inline]
    pub fn window(&self) -> usize {
        self.window
    }

    #[inline]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The full physical column. Index it with `selection().physical(i)`, or go
    /// through `value` for a single row.
    #[inline]
    pub fn column(&self, i: usize) -> &Column {
        &self.columns[i]
    }

    pub fn columns(&self) -> &[Arc<Column>] {
        &self.columns
    }

    #[inline]
    pub fn value(&self, column: usize, logical_row: usize) -> crate::types::ScalarValue {
        self.columns[column].value(self.selection.physical(logical_row))
    }

    /// The same columns under a narrower selection.
    pub fn with_selection(&self, selection: Selection) -> Batch {
        Batch {
            schema: Arc::clone(&self.schema),
            columns: self.columns.clone(),
            selection,
            column_len: self.column_len,
            window: self.window,
        }
    }

    /// Keep the logical positions named by `keep`, composing with any selection
    /// already in place.
    pub fn filter(&self, keep: &[u32]) -> Batch {
        self.with_selection(self.selection.select(keep))
    }

    /// Materialize the live rows into dense columns, discarding the selection.
    ///
    /// Only worth doing when the selection has become sparse enough that the
    /// gather costs more than this copy -- see `Selection::should_compact`.
    pub fn compact(&self) -> Batch {
        if self.selection.is_contiguous() && self.num_rows() == self.column_len {
            return self.clone();
        }
        let indices: Vec<usize> = self.selection.iter().collect();
        let columns: Vec<Arc<Column>> = self
            .columns
            .iter()
            .map(|c| Arc::new(c.take(&indices)))
            .collect();
        let len = indices.len();
        Batch {
            schema: Arc::clone(&self.schema),
            columns,
            selection: Selection::full(len),
            column_len: len,
            window: len,
        }
    }

    /// A contiguous window of the live rows, used by LIMIT / OFFSET.
    pub fn slice(&self, offset: usize, len: usize) -> Batch {
        debug_assert!(offset + len <= self.num_rows());
        let selection = match &self.selection {
            Selection::Range { offset: base, .. } => Selection::Range {
                offset: base + offset,
                len,
            },
            Selection::Indices(v) => {
                Selection::Indices(Arc::new(v[offset..offset + len].to_vec()))
            }
        };
        self.with_selection(selection)
    }

    /// Bytes of column data this batch keeps alive. Shared with whatever else
    /// holds the same `Arc`s, so this is an upper bound on what it costs.
    pub fn byte_size(&self) -> usize {
        self.columns.iter().map(|c| c.byte_size()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::column::{ColumnBuilder, ColumnData};
    use crate::storage::schema::Field;
    use crate::types::{DataType, ScalarValue};

    fn batch_of(values: &[i64]) -> Batch {
        let mut b = ColumnBuilder::new(&DataType::Int64);
        for v in values {
            b.append(&ScalarValue::Int64(*v)).unwrap();
        }
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
        Batch::dense(schema, vec![b.finish()])
    }

    #[test]
    fn filtering_composes_without_copying_columns() {
        let batch = batch_of(&[10, 20, 30, 40, 50]);
        let physical = batch.column(0) as *const _;

        let once = batch.filter(&[1, 2, 3, 4]);
        let twice = once.filter(&[1, 3]);

        assert_eq!(twice.num_rows(), 2);
        assert_eq!(twice.value(0, 0), ScalarValue::Int64(30));
        assert_eq!(twice.value(0, 1), ScalarValue::Int64(50));
        // The column itself was never rewritten.
        assert_eq!(twice.column(0) as *const _, physical);
        assert_eq!(twice.column_len(), 5);
        // The window is what the batch spanned before filtering, not what
        // survived -- that is the denominator the compaction decision needs.
        assert_eq!(twice.window(), 5);
    }

    #[test]
    fn filtering_nothing_keeps_the_contiguous_fast_path() {
        let batch = batch_of(&[1, 2, 3]);
        let same = batch.filter(&[0, 1, 2]);
        assert!(same.selection().is_contiguous());
    }

    #[test]
    fn compact_materializes_and_resets_the_selection() {
        let batch = batch_of(&[10, 20, 30, 40, 50]).filter(&[0, 4]);
        assert!(!batch.selection().is_contiguous());

        let dense = batch.compact();
        assert!(dense.selection().is_contiguous());
        assert_eq!(dense.column_len(), 2);
        assert_eq!(dense.num_rows(), 2);
        assert_eq!(dense.value(0, 0), ScalarValue::Int64(10));
        assert_eq!(dense.value(0, 1), ScalarValue::Int64(50));
        assert_eq!(dense.column(0).len(), 2);
    }

    #[test]
    fn compaction_triggers_only_when_the_selection_is_sparse() {
        let sparse = Selection::Indices(Arc::new(vec![1, 2, 3]));
        assert!(sparse.should_compact(2048, 0.2));
        assert!(!sparse.should_compact(4, 0.2));
        // A contiguous range is already dense; there is nothing to gain.
        assert!(!Selection::full(3).should_compact(2048, 0.2));
    }

    #[test]
    fn a_mostly_full_batch_is_not_considered_sparse() {
        // 1935 of 2048 rows surviving is not a case for compaction. Measuring
        // this against the row-group length rather than the batch window is
        // what made every filter compact.
        let keep: Vec<u32> = (0..2048u32).filter(|i| i % 18 != 0).collect();
        let selection = Selection::full(2048).select(&keep);
        assert!(!selection.should_compact(2048, 0.2));
    }

    #[test]
    fn slice_windows_live_rows_not_physical_ones() {
        let batch = batch_of(&[10, 20, 30, 40, 50]).filter(&[1, 3, 4]);
        let window = batch.slice(1, 2);
        assert_eq!(window.num_rows(), 2);
        assert_eq!(window.value(0, 0), ScalarValue::Int64(40));
        assert_eq!(window.value(0, 1), ScalarValue::Int64(50));
    }

    #[test]
    fn a_batch_can_have_rows_but_no_columns() {
        let b = Batch::empty_rows(Arc::new(Schema::empty()), 1);
        assert_eq!(b.num_rows(), 1);
        assert!(b.columns().is_empty());
    }

    #[test]
    fn dense_batch_reports_its_column_type() {
        let b = batch_of(&[1]);
        assert!(matches!(b.column(0).data, ColumnData::Int64(_)));
    }
}
