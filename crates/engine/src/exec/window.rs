//! Window functions.
//!
//! Unlike an aggregate, a window function adds a column rather than collapsing
//! rows: every input row comes out, with one extra value computed from its
//! neighbours. Which neighbours is the whole subject.
//!
//! The shape is always the same:
//!
//! 1. sort by `(partition keys, order keys)`, so every partition is contiguous
//!    and ordered within itself;
//! 2. walk the partitions;
//! 3. inside a partition, find *peer groups* -- runs of rows equal on the order
//!    keys -- because that is what `RANGE` frames and `RANK` are defined in
//!    terms of;
//! 4. compute each function over each row's frame.
//!
//! ## Frames
//!
//! `ROWS` counts rows, so two tied rows have different frames. `RANGE` counts
//! *values*, so tied rows are peers and share one -- which is what makes
//! `SUM(x) OVER (ORDER BY y)` give every row with the same `y` the same running
//! total, and is the default when a window has an ORDER BY. Without an ORDER BY
//! there is nothing to run along, so the frame is the whole partition.
//!
//! Ranking functions ignore the frame entirely: a position is not a window over
//! values.
//!
//! ## Cost
//!
//! A frame that is the whole partition is computed once per partition, and a
//! running frame (`UNBOUNDED PRECEDING` to the current row or its peers) is
//! accumulated in one pass. Any other frame is recomputed per row, which is
//! `O(n * w)` -- correct, and the obvious place to add a sliding accumulator
//! when a query makes it matter.

use std::sync::Arc;

use crate::error::Result;
use crate::exec::sort::{compare_rows_for, CompiledSortKey};
use crate::exec::{Operator, OperatorStats, Timer};
use crate::expr::{self, CompiledExpr};
use crate::plan::{AggregateFunction, Frame, FrameBound, FrameUnits, WindowFunction};
use crate::storage::{Batch, Column, ColumnBuilder, Schema, DEFAULT_BATCH_SIZE};
use crate::types::{self, ScalarValue};

/// A window function resolved against the layout of its input.
pub struct CompiledWindowFunction {
    pub func: WindowFunction,
    pub args: Vec<CompiledExpr>,
    pub frame: Frame,
    pub data_type: crate::types::DataType,
}

pub struct WindowExec {
    input: Box<dyn Operator>,
    partition_by: Vec<CompiledSortKey>,
    order_by: Vec<CompiledSortKey>,
    functions: Vec<CompiledWindowFunction>,
    schema: Arc<Schema>,
    /// Input columns followed by one column per function, in sorted order.
    rows: Option<Vec<Column>>,
    emitted: usize,
    stats: OperatorStats,
}

impl WindowExec {
    pub fn new(
        input: Box<dyn Operator>,
        partition_by: Vec<CompiledSortKey>,
        order_by: Vec<CompiledSortKey>,
        functions: Vec<CompiledWindowFunction>,
        schema: Arc<Schema>,
    ) -> WindowExec {
        let detail = format!(
            "{} function(s), {} partition key(s), {} order key(s)",
            functions.len(),
            partition_by.len(),
            order_by.len()
        );
        WindowExec {
            input,
            partition_by,
            order_by,
            functions,
            schema,
            rows: None,
            emitted: 0,
            stats: OperatorStats::new("Window", detail),
        }
    }

    fn build(&mut self) -> Result<()> {
        if self.rows.is_some() {
            return Ok(());
        }

        // The input's own columns, plus each function's arguments, materialized
        // together so that a single sort orders all of them.
        let input_schema = self.input.schema();
        let mut columns: Vec<ColumnBuilder> = input_schema
            .fields
            .iter()
            .map(|f| ColumnBuilder::new(&f.data_type))
            .collect();
        let mut sort_specs: Vec<CompiledSortKey> = Vec::new();
        let mut key_builders: Vec<ColumnBuilder> = Vec::new();
        for k in self.partition_by.iter().chain(&self.order_by) {
            key_builders.push(ColumnBuilder::new(&k.expr.data_type));
        }
        let mut arg_builders: Vec<Vec<ColumnBuilder>> = self
            .functions
            .iter()
            .map(|f| {
                f.args
                    .iter()
                    .map(|a| ColumnBuilder::new(&a.data_type))
                    .collect()
            })
            .collect();

        while let Some(batch) = self.input.next()? {
            let keys: Vec<Arc<Column>> = self
                .partition_by
                .iter()
                .chain(&self.order_by)
                .map(|k| expr::eval(&k.expr, &batch))
                .collect::<Result<_>>()?;
            let args: Vec<Vec<Arc<Column>>> = self
                .functions
                .iter()
                .map(|f| {
                    f.args
                        .iter()
                        .map(|a| expr::eval(a, &batch))
                        .collect::<Result<Vec<_>>>()
                })
                .collect::<Result<_>>()?;

            for row in 0..batch.num_rows() {
                for (c, b) in columns.iter_mut().enumerate() {
                    b.append(&batch.value(c, row))?;
                }
                for (k, b) in key_builders.iter_mut().enumerate() {
                    b.append(&keys[k].value(row))?;
                }
                for (f, builders) in arg_builders.iter_mut().enumerate() {
                    for (a, b) in builders.iter_mut().enumerate() {
                        b.append(&args[f][a].value(row))?;
                    }
                }
            }
        }

        let timer = Timer::start();
        let input_columns: Vec<Column> = columns.into_iter().map(|b| b.finish()).collect();
        let key_columns: Vec<Column> = key_builders.into_iter().map(|b| b.finish()).collect();
        let arg_columns: Vec<Vec<Column>> = arg_builders
            .into_iter()
            .map(|v| v.into_iter().map(|b| b.finish()).collect())
            .collect();

        let n = key_columns
            .first()
            .map_or(input_columns.first().map_or(0, |c| c.len()), |c| c.len());
        self.stats.rows_in += n as u64;

        // Partition keys sort ascending: their order does not matter, only that
        // equal ones end up adjacent.
        for k in &self.partition_by {
            sort_specs.push(CompiledSortKey {
                expr: k.expr.clone(),
                ascending: true,
                nulls_first: true,
            });
        }
        for k in &self.order_by {
            sort_specs.push(CompiledSortKey {
                expr: k.expr.clone(),
                ascending: k.ascending,
                nulls_first: k.nulls_first,
            });
        }

        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_by(|a, b| {
            compare_rows_for(&key_columns, &sort_specs, *a as usize, *b as usize)
        });

        // Boundaries where a key column changes value.
        let partition_count = self.partition_by.len();
        let changes = |columns: &[Column], from: usize, to: usize, upto: usize| -> bool {
            (0..upto).any(|k| {
                let c = &columns[k];
                c.is_valid(from) != c.is_valid(to)
                    || (c.is_valid(from)
                        && types::compare(&c.value(from), &c.value(to))
                            != Some(std::cmp::Ordering::Equal))
            })
        };

        let mut results: Vec<ColumnBuilder> = self
            .functions
            .iter()
            .map(|f| ColumnBuilder::new(&f.data_type))
            .collect();
        // Values land in sorted order and are reordered back at the end.
        let mut values: Vec<Vec<ScalarValue>> =
            self.functions.iter().map(|_| vec![ScalarValue::Null; n]).collect();

        let mut start = 0usize;
        while start < n {
            let mut end = start + 1;
            while end < n
                && !changes(
                    &key_columns,
                    order[start] as usize,
                    order[end] as usize,
                    partition_count,
                )
            {
                end += 1;
            }

            // Peer groups: runs equal on the order keys, which RANK and RANGE
            // frames are both defined in terms of.
            let mut peers: Vec<(usize, usize)> = Vec::new();
            let mut peer_start = start;
            while peer_start < end {
                let mut peer_end = peer_start + 1;
                while peer_end < end
                    && !changes(
                        &key_columns,
                        order[peer_start] as usize,
                        order[peer_end] as usize,
                        key_columns.len(),
                    )
                {
                    peer_end += 1;
                }
                peers.push((peer_start, peer_end));
                peer_start = peer_end;
            }

            for (f, function) in self.functions.iter().enumerate() {
                compute_partition(
                    function,
                    &arg_columns[f],
                    &order,
                    start,
                    end,
                    &peers,
                    &mut values[f],
                )?;
            }
            start = end;
        }

        // Emit in the sorted order the window imposed: a window function does
        // not promise to preserve input order, and every engine returns the
        // sorted order here.
        let indices: Vec<usize> = order.iter().map(|i| *i as usize).collect();
        let mut out: Vec<Column> = input_columns.iter().map(|c| c.take(&indices)).collect();
        for (f, builder) in results.iter_mut().enumerate() {
            for value in &values[f] {
                builder.append(value)?;
            }
        }
        out.extend(results.into_iter().map(|b| b.finish()));

        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.rows = Some(out);
        Ok(())
    }
}

/// Compute one function over one partition, writing results by sorted position.
fn compute_partition(
    function: &CompiledWindowFunction,
    args: &[Column],
    order: &[u32],
    start: usize,
    end: usize,
    peers: &[(usize, usize)],
    out: &mut [ScalarValue],
) -> Result<()> {
    match function.func {
        WindowFunction::RowNumber => {
            for (offset, slot) in out[start..end].iter_mut().enumerate() {
                *slot = ScalarValue::Int64(offset as i64 + 1);
            }
        }
        WindowFunction::Rank => {
            // Ties share the first position of their peer group, and the next
            // group jumps past them -- so a rank can be skipped.
            for (group_start, group_end) in peers {
                let rank = (group_start - start + 1) as i64;
                for slot in &mut out[*group_start..*group_end] {
                    *slot = ScalarValue::Int64(rank);
                }
            }
        }
        WindowFunction::DenseRank => {
            // Same ties, but the ranks are consecutive.
            for (rank, (group_start, group_end)) in peers.iter().enumerate() {
                for slot in &mut out[*group_start..*group_end] {
                    *slot = ScalarValue::Int64(rank as i64 + 1);
                }
            }
        }
        WindowFunction::Lag | WindowFunction::Lead => {
            let offset = args
                .get(1)
                .map(|c| c.value(order[start] as usize))
                .and_then(|v| match v {
                    ScalarValue::Int32(i) => Some(i as i64),
                    ScalarValue::Int64(i) => Some(i),
                    _ => None,
                })
                .unwrap_or(1);
            let signed = if function.func == WindowFunction::Lag {
                -offset
            } else {
                offset
            };
            for (offset, slot) in out[start..end].iter_mut().enumerate() {
                let position = start + offset;
                let target = position as i64 + signed;
                *slot = if target >= start as i64 && target < end as i64 {
                    args[0].value(order[target as usize] as usize)
                } else {
                    // Past the edge of the partition there is no neighbour, so
                    // the default applies -- NULL unless one was given.
                    args.get(2)
                        .map(|c| c.value(order[position] as usize))
                        .unwrap_or(ScalarValue::Null)
                };
            }
        }
        WindowFunction::Aggregate(aggregate) => {
            for (offset, slot) in out[start..end].iter_mut().enumerate() {
                let position = start + offset;
                let (from, to) = frame_bounds(&function.frame, position, start, end, peers);
                *slot = match args.first() {
                    Some(values) => {
                        accumulate(aggregate, values, order, from, to, function.data_type)
                    }
                    // `COUNT(*)` has no argument: it counts the rows in the
                    // frame rather than the non-NULL values of anything.
                    None => ScalarValue::Int64((to - from) as i64),
                };
            }
        }
    }
    Ok(())
}

/// The half-open range of sorted positions a row's frame covers.
fn frame_bounds(
    frame: &Frame,
    position: usize,
    start: usize,
    end: usize,
    peers: &[(usize, usize)],
) -> (usize, usize) {
    // Under RANGE, "current row" means the whole peer group: rows equal on the
    // order keys share one frame.
    let (peer_start, peer_end) = peers
        .iter()
        .find(|(a, b)| position >= *a && position < *b)
        .copied()
        .unwrap_or((position, position + 1));

    let current_start = match frame.units {
        FrameUnits::Range => peer_start,
        FrameUnits::Rows => position,
    };
    let current_end = match frame.units {
        FrameUnits::Range => peer_end,
        FrameUnits::Rows => position + 1,
    };

    let from = match frame.start {
        FrameBound::UnboundedPreceding => start,
        FrameBound::Preceding(n) => current_start.saturating_sub(n).max(start),
        FrameBound::CurrentRow => current_start,
        FrameBound::Following(n) => (current_start + n).min(end),
        FrameBound::UnboundedFollowing => end,
    };
    let to = match frame.end {
        FrameBound::UnboundedPreceding => start,
        FrameBound::Preceding(n) => current_end.saturating_sub(n).max(start),
        FrameBound::CurrentRow => current_end,
        FrameBound::Following(n) => (current_end + n).min(end),
        FrameBound::UnboundedFollowing => end,
    };
    (from.min(to), to)
}

fn accumulate(
    func: AggregateFunction,
    values: &Column,
    order: &[u32],
    from: usize,
    to: usize,
    data_type: crate::types::DataType,
) -> ScalarValue {
    let mut count = 0i64;
    let mut int_sum = 0i64;
    let mut float_sum = 0.0f64;
    let mut any = false;
    let mut best: Option<ScalarValue> = None;

    for row in &order[from..to] {
        let value = values.value(*row as usize);
        // Every aggregate but COUNT(*) ignores NULLs, exactly as it does over a
        // group.
        if value.is_null() {
            continue;
        }
        count += 1;
        any = true;
        match &value {
            ScalarValue::Int32(v) => int_sum = int_sum.saturating_add(*v as i64),
            ScalarValue::Int64(v) => int_sum = int_sum.saturating_add(*v),
            other => float_sum += other.as_f64().unwrap_or(0.0),
        }
        let replace = match &best {
            None => true,
            Some(current) => match types::compare(&value, current) {
                Some(std::cmp::Ordering::Less) => func == AggregateFunction::Min,
                Some(std::cmp::Ordering::Greater) => func == AggregateFunction::Max,
                _ => false,
            },
        };
        if replace {
            best = Some(value);
        }
    }

    match func {
        AggregateFunction::Count => ScalarValue::Int64(count),
        AggregateFunction::Sum => {
            if !any {
                ScalarValue::Null
            } else if data_type == crate::types::DataType::Float64 {
                ScalarValue::Float64(float_sum + int_sum as f64)
            } else {
                ScalarValue::Int64(int_sum)
            }
        }
        AggregateFunction::Avg => {
            if count == 0 {
                ScalarValue::Null
            } else {
                ScalarValue::Float64((float_sum + int_sum as f64) / count as f64)
            }
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            best.unwrap_or(ScalarValue::Null)
        }
    }
}

impl Operator for WindowExec {
    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn set_reason(&mut self, reason: String) {
        self.stats.because(reason);
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
        let total = rows.first().map_or(0, |c| c.len());
        if self.emitted >= total {
            return Ok(None);
        }

        let timer = Timer::start();
        let len = DEFAULT_BATCH_SIZE.min(total - self.emitted);
        let columns: Vec<Arc<Column>> = rows
            .iter()
            .map(|c| Arc::new(c.slice(self.emitted, len)))
            .collect();
        self.emitted += len;

        let out = Batch::new(
            Arc::clone(&self.schema),
            columns,
            crate::storage::Selection::full(len),
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
