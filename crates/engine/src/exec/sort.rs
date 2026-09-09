//! Sorting.
//!
//! Two operators for one job, and the physical planner picks between them:
//! [`SortExec`] orders everything, [`TopNExec`] keeps only the k smallest. That
//! is the difference between `O(n log n)` over a million rows and `O(n log k)`
//! over a heap of ten, and a `LIMIT` above a sort is what makes the second one
//! available.
//!
//! ## NULL placement
//!
//! SQL leaves it implementation-defined where NULLs sort when the query does
//! not say. This engine follows SQLite -- NULLs first ascending, last
//! descending -- because SQLite is what the differential corpus compares
//! against. PostgreSQL chose the opposite default, so a query that relies on it
//! is relying on something the standard does not promise.
//!
//! ## Spilling
//!
//! Everything is sorted in memory. The shape is the one an external sort would
//! use -- accumulate, then order an index vector, then gather -- so spilling
//! would slot in as run generation at the accumulate step and a k-way merge at
//! the gather step, without the comparison logic changing. There is no spilling
//! today, and a sort larger than memory will fail rather than get slow.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::error::Result;
use crate::exec::{OperatorStats, Operator, Timer};
use crate::expr::{self, CompiledExpr};
use crate::storage::{Batch, Column, ColumnBuilder, ColumnData, Schema, DEFAULT_BATCH_SIZE};

/// A sort key, resolved against the batches it will see.
pub struct CompiledSortKey {
    pub expr: CompiledExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// Every row of the input, materialized, plus its key values.
struct Sorted {
    columns: Vec<Column>,
    keys: Vec<Column>,
    order: Vec<u32>,
}

/// Compare row `a` against row `b` on one key column.
///
/// Typed rather than going through `ScalarValue`, so a string comparison reads
/// the offset buffer instead of allocating two `String`s per comparison -- and
/// a sort does `n log n` of them.
fn compare_values(column: &Column, a: usize, b: usize) -> Ordering {
    match &column.data {
        ColumnData::Int32(v) => v[a].cmp(&v[b]),
        ColumnData::Int64(v) => v[a].cmp(&v[b]),
        ColumnData::Date32(v) => v[a].cmp(&v[b]),
        ColumnData::Timestamp(v) => v[a].cmp(&v[b]),
        ColumnData::Boolean(v) => v.get(a).cmp(&v.get(b)),
        ColumnData::Utf8(v) => v.get(a).cmp(v.get(b)),
        ColumnData::Float64(v) => {
            // NaN has no place in a total order, and a sort needs one. Treating
            // it as larger than everything is what SQLite and PostgreSQL both
            // do, and it at least makes the result deterministic.
            v[a].partial_cmp(&v[b]).unwrap_or_else(|| {
                v[a].is_nan().cmp(&v[b].is_nan())
            })
        }
        ColumnData::Decimal128 { values, .. } => values[a].cmp(&values[b]),
        ColumnData::Null(_) => Ordering::Equal,
    }
}

/// Compare two rows on a list of keys. Shared with the window operator, which
/// sorts by partition and order keys together.
pub fn compare_rows_for(
    keys: &[Column],
    specs: &[CompiledSortKey],
    a: usize,
    b: usize,
) -> Ordering {
    compare_rows(keys, specs, a, b)
}

fn compare_rows(keys: &[Column], specs: &[CompiledSortKey], a: usize, b: usize) -> Ordering {
    for (column, spec) in keys.iter().zip(specs) {
        let (a_valid, b_valid) = (column.is_valid(a), column.is_valid(b));
        let ordering = match (a_valid, b_valid) {
            (true, true) => {
                let base = compare_values(column, a, b);
                if spec.ascending {
                    base
                } else {
                    base.reverse()
                }
            }
            // A NULL's position is fixed by `nulls_first` and is *not* flipped
            // by the direction -- the direction has already been folded into
            // the default.
            (false, false) => Ordering::Equal,
            (false, true) => {
                if spec.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (true, false) => {
                if spec.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Drain an operator into one dense set of columns plus the key columns.
fn materialize(
    input: &mut dyn Operator,
    keys: &[CompiledSortKey],
    schema: &Arc<Schema>,
) -> Result<(Vec<Column>, Vec<Column>)> {
    let mut builders: Vec<ColumnBuilder> = schema
        .fields
        .iter()
        .map(|f| ColumnBuilder::new(&f.data_type))
        .collect();
    let mut key_builders: Vec<ColumnBuilder> = keys
        .iter()
        .map(|k| ColumnBuilder::new(&k.expr.data_type))
        .collect();

    while let Some(batch) = input.next()? {
        let key_columns: Vec<Arc<Column>> = keys
            .iter()
            .map(|k| expr::eval(&k.expr, &batch))
            .collect::<Result<_>>()?;
        for row in 0..batch.num_rows() {
            for (c, b) in builders.iter_mut().enumerate() {
                b.append(&batch.value(c, row))?;
            }
            for (k, b) in key_builders.iter_mut().enumerate() {
                b.append(&key_columns[k].value(row))?;
            }
        }
    }
    Ok((
        builders.into_iter().map(|b| b.finish()).collect(),
        key_builders.into_iter().map(|b| b.finish()).collect(),
    ))
}

pub struct SortExec {
    input: Box<dyn Operator>,
    keys: Vec<CompiledSortKey>,
    schema: Arc<Schema>,
    sorted: Option<Sorted>,
    emitted: usize,
    stats: OperatorStats,
}

impl SortExec {
    pub fn new(
        input: Box<dyn Operator>,
        keys: Vec<CompiledSortKey>,
        schema: Arc<Schema>,
    ) -> SortExec {
        let detail = format!("full sort on {} key(s)", keys.len());
        SortExec {
            input,
            keys,
            schema,
            sorted: None,
            emitted: 0,
            stats: OperatorStats::new("Sort", detail),
        }
    }

    fn build(&mut self) -> Result<()> {
        if self.sorted.is_some() {
            return Ok(());
        }
        let (columns, keys) = materialize(self.input.as_mut(), &self.keys, &self.schema)?;
        let timer = Timer::start();
        let rows = keys.first().map_or(
            columns.first().map_or(0, |c| c.len()),
            |k| k.len(),
        );
        self.stats.rows_in += rows as u64;

        let mut order: Vec<u32> = (0..rows as u32).collect();
        // `sort_by` is a stable merge sort, which matters: rows that compare
        // equal keep their input order, so a sort on a non-unique key is
        // reproducible rather than arbitrary.
        order.sort_by(|a, b| compare_rows(&keys, &self.keys, *a as usize, *b as usize));

        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.sorted = Some(Sorted { columns, keys, order });
        Ok(())
    }
}

impl Operator for SortExec {
    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        self.build()?;
        let sorted = self.sorted.as_ref().expect("built above");
        if self.emitted >= sorted.order.len() {
            return Ok(None);
        }

        let timer = Timer::start();
        let end = (self.emitted + DEFAULT_BATCH_SIZE).min(sorted.order.len());
        let indices: Vec<usize> = sorted.order[self.emitted..end]
            .iter()
            .map(|i| *i as usize)
            .collect();
        self.emitted = end;

        let columns: Vec<Column> = sorted.columns.iter().map(|c| c.take(&indices)).collect();
        let out = Batch::new(
            Arc::clone(&self.schema),
            columns.into_iter().map(Arc::new).collect(),
            crate::storage::Selection::full(indices.len()),
        );
        let _ = &sorted.keys;
        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.stats.record_output(&out);
        Ok(Some(out))
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}

// ---------------------------------------------------------------------------
// Top-N
// ---------------------------------------------------------------------------

/// One candidate row in the heap: its key values and the row itself.
struct Candidate {
    keys: Vec<crate::types::ScalarValue>,
    row: Vec<crate::types::ScalarValue>,
}

/// Orders candidates so that the *worst* one is the heap's maximum, which is
/// the one to evict when the heap is over size.
struct Ranked<'a> {
    candidate: Candidate,
    specs: &'a [CompiledSortKey],
}

impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked<'_> {}
impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_key_values(&self.candidate.keys, &other.candidate.keys, self.specs)
    }
}

/// Compare two rows' key values under the sort specification.
///
/// Split out from `Ord` so the heap's admission test can use it *before* a
/// candidate exists: for a top-10 of a million rows, all but ten are rejected,
/// and materializing a row that is about to be thrown away is the whole cost.
fn compare_key_values(
    a: &[crate::types::ScalarValue],
    b: &[crate::types::ScalarValue],
    specs: &[CompiledSortKey],
) -> Ordering {
    for (i, spec) in specs.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let ordering = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (false, false) => {
                let base = crate::types::compare(x, y).unwrap_or(Ordering::Equal);
                if spec.ascending {
                    base
                } else {
                    base.reverse()
                }
            }
            (true, false) => {
                if spec.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if spec.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// A sort with a limit above it: keep only the rows that could still make the
/// cut.
///
/// The heap holds at most `k` rows, so memory is bounded by the limit rather
/// than by the input -- the point of the operator. It also means a top-10 over
/// a million rows never materializes the million.
pub struct TopNExec {
    input: Box<dyn Operator>,
    keys: Vec<CompiledSortKey>,
    schema: Arc<Schema>,
    /// Rows to keep: `skip + fetch`, since the skipped ones still have to be
    /// ranked before they can be discarded.
    limit: usize,
    rows: Option<Vec<Candidate>>,
    emitted: usize,
    stats: OperatorStats,
}

impl TopNExec {
    pub fn new(
        input: Box<dyn Operator>,
        keys: Vec<CompiledSortKey>,
        schema: Arc<Schema>,
        limit: usize,
    ) -> TopNExec {
        let detail = format!("top-{limit} heap on {} key(s)", keys.len());
        TopNExec {
            input,
            keys,
            schema,
            limit,
            rows: None,
            emitted: 0,
            stats: OperatorStats::new("TopN", detail),
        }
    }

    fn build(&mut self) -> Result<()> {
        if self.rows.is_some() {
            return Ok(());
        }
        let mut heap: BinaryHeap<Ranked> = BinaryHeap::with_capacity(self.limit + 1);

        while let Some(batch) = self.input.next()? {
            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;
            let key_columns: Vec<Arc<Column>> = self
                .keys
                .iter()
                .map(|k| expr::eval(&k.expr, &batch))
                .collect::<Result<_>>()?;

            for row in 0..batch.num_rows() {
                let keys: Vec<crate::types::ScalarValue> =
                    key_columns.iter().map(|c| c.value(row)).collect();

                // Decide on the keys alone. Building the row is what costs --
                // one `ScalarValue` per column, strings included -- and for a
                // top-10 of a million rows it would be paid a million times for
                // ten rows that survive.
                let full = heap.len() < self.limit;
                if !full {
                    match heap.peek() {
                        Some(worst)
                            if compare_key_values(&keys, &worst.candidate.keys, &self.keys)
                                != Ordering::Less =>
                        {
                            continue
                        }
                        None => continue,
                        _ => {}
                    }
                }

                let candidate = Candidate {
                    keys,
                    row: (0..batch.columns().len())
                        .map(|c| batch.value(c, row))
                        .collect(),
                };
                let ranked = Ranked {
                    candidate,
                    specs: &self.keys,
                };
                if !full {
                    heap.pop();
                }
                heap.push(ranked);
            }
            self.stats.elapsed_nanos += timer.elapsed_nanos();
        }

        let timer = Timer::start();
        // The heap yields worst-first, so reversing gives sorted order.
        let mut rows: Vec<Candidate> = Vec::with_capacity(heap.len());
        while let Some(r) = heap.pop() {
            rows.push(r.candidate);
        }
        rows.reverse();
        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.rows = Some(rows);
        Ok(())
    }
}

impl Operator for TopNExec {
    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        self.build()?;
        let rows = self.rows.as_ref().expect("built above");
        if self.emitted >= rows.len() {
            return Ok(None);
        }

        let timer = Timer::start();
        let end = (self.emitted + DEFAULT_BATCH_SIZE).min(rows.len());
        let mut builders: Vec<ColumnBuilder> = self
            .schema
            .fields
            .iter()
            .map(|f| ColumnBuilder::new(&f.data_type))
            .collect();
        for candidate in &rows[self.emitted..end] {
            for (c, b) in builders.iter_mut().enumerate() {
                b.append(&candidate.row[c])?;
            }
        }
        self.emitted = end;

        let out = Batch::dense(
            Arc::clone(&self.schema),
            builders.into_iter().map(|b| b.finish()).collect(),
        );
        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.stats.record_output(&out);
        Ok(Some(out))
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}
