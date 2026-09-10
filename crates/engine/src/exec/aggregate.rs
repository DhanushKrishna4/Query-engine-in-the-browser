//! Hash aggregation.
//!
//! One pass over the input building a hash table from grouping key to a set of
//! accumulators, then one pass over the table emitting results. Groups are
//! emitted in first-seen order rather than hash order, so the same query over
//! the same data always produces the same row order -- a `HashMap`'s iteration
//! order would not.
//!
//! ## Two things that are easy to get wrong
//!
//! **NULL is a group.** Unlike an equi-join, `GROUP BY` puts every NULL in one
//! group together: grouping asks "are these the same value?", where NULL is
//! indistinguishable from NULL, while a join asks "are these equal?", where
//! `NULL = NULL` is unknown. [`crate::exec::key`] has the same note.
//!
//! **An empty input is not an empty answer.** A query with no GROUP BY produces
//! exactly one row even over zero input rows, and in that row COUNT is 0 while
//! SUM, AVG, MIN and MAX are all NULL. Adding a GROUP BY changes that: with no
//! rows there are no groups, so the result is genuinely empty.
//!
//! A sort-based aggregate, which streams without a hash table when the input is
//! already grouped, is the other implementation the physical planner will
//! eventually choose between. It needs sorted input, so it waits for Sort.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::{Diagnostic, Result};
use crate::exec::key::KeyValue;
use crate::exec::{OperatorStats, Timer};
use crate::expr::{self, CompiledExpr};
use crate::plan::AggregateFunction;
use crate::storage::{Batch, Column, ColumnBuilder, Schema, DEFAULT_BATCH_SIZE};
use crate::types::{self, DataType, ScalarValue};

use super::Operator;

/// An aggregate, resolved against the layout of its input.
pub struct CompiledAggregate {
    pub func: AggregateFunction,
    /// `None` for `COUNT(*)`, which counts rows rather than values.
    pub arg: Option<CompiledExpr>,
    pub distinct: bool,
    pub data_type: DataType,
}

enum Accumulator {
    Count {
        n: u64,
        seen: Option<HashSet<KeyValue>>,
    },
    Sum {
        /// Integer and floating sums are kept apart so that an integer SUM stays
        /// exact instead of drifting through f64.
        int: i64,
        float: f64,
        any: bool,
        seen: Option<HashSet<KeyValue>>,
    },
    Avg {
        sum: f64,
        n: u64,
        seen: Option<HashSet<KeyValue>>,
    },
    MinMax {
        best: Option<ScalarValue>,
        is_min: bool,
    },
}

impl Accumulator {
    fn new(agg: &CompiledAggregate) -> Accumulator {
        let seen = agg.distinct.then(HashSet::new);
        match agg.func {
            AggregateFunction::Count => Accumulator::Count { n: 0, seen },
            AggregateFunction::Sum => Accumulator::Sum {
                int: 0,
                float: 0.0,
                any: false,
                seen,
            },
            AggregateFunction::Avg => Accumulator::Avg { sum: 0.0, n: 0, seen },
            AggregateFunction::Min => Accumulator::MinMax {
                best: None,
                is_min: true,
            },
            AggregateFunction::Max => Accumulator::MinMax {
                best: None,
                is_min: false,
            },
        }
    }

    /// Feed one value. `None` means `COUNT(*)`, which counts the row itself.
    fn update(&mut self, value: Option<&ScalarValue>) -> Result<()> {
        // Every aggregate but COUNT(*) ignores NULL inputs entirely -- that is
        // what makes SUM over a column of NULLs return NULL rather than 0.
        let v = match value {
            None => {
                if let Accumulator::Count { n, .. } = self {
                    *n += 1;
                }
                return Ok(());
            }
            Some(v) if v.is_null() => return Ok(()),
            Some(v) => v,
        };

        // DISTINCT filters before the accumulator ever sees the value.
        if let Some(seen) = self.seen_mut() {
            if !seen.insert(KeyValue::of(v)) {
                return Ok(());
            }
        }

        match self {
            Accumulator::Count { n, .. } => *n += 1,
            Accumulator::Sum { int, float, any, .. } => {
                *any = true;
                match v {
                    ScalarValue::Int32(x) => {
                        *int = int
                            .checked_add(*x as i64)
                            .ok_or_else(|| Diagnostic::exec("arithmetic overflow in SUM"))?
                    }
                    ScalarValue::Int64(x) => {
                        *int = int
                            .checked_add(*x)
                            .ok_or_else(|| Diagnostic::exec("arithmetic overflow in SUM"))?
                    }
                    other => *float += other.as_f64().unwrap_or(0.0),
                }
            }
            Accumulator::Avg { sum, n, .. } => {
                *sum += v.as_f64().unwrap_or(0.0);
                *n += 1;
            }
            Accumulator::MinMax { best, is_min } => {
                let replace = match best {
                    None => true,
                    Some(cur) => match types::compare(v, cur) {
                        Some(std::cmp::Ordering::Less) => *is_min,
                        Some(std::cmp::Ordering::Greater) => !*is_min,
                        // Unordered (NaN) never displaces the incumbent.
                        _ => false,
                    },
                };
                if replace {
                    *best = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    fn seen_mut(&mut self) -> Option<&mut HashSet<KeyValue>> {
        match self {
            Accumulator::Count { seen, .. }
            | Accumulator::Sum { seen, .. }
            | Accumulator::Avg { seen, .. } => seen.as_mut(),
            Accumulator::MinMax { .. } => None,
        }
    }

    fn finish(&self, data_type: DataType) -> ScalarValue {
        match self {
            Accumulator::Count { n, .. } => ScalarValue::Int64(*n as i64),
            Accumulator::Sum { int, float, any, .. } => {
                if !*any {
                    // No non-NULL input: the sum is unknown, not zero.
                    return ScalarValue::Null;
                }
                match data_type {
                    DataType::Float64 => ScalarValue::Float64(*float + *int as f64),
                    _ => ScalarValue::Int64(*int),
                }
            }
            Accumulator::Avg { sum, n, .. } => {
                if *n == 0 {
                    ScalarValue::Null
                } else {
                    ScalarValue::Float64(sum / *n as f64)
                }
            }
            Accumulator::MinMax { best, .. } => best.clone().unwrap_or(ScalarValue::Null),
        }
    }
}

pub struct HashAggregateExec {
    input: Box<dyn Operator>,
    group_exprs: Vec<CompiledExpr>,
    aggregates: Vec<CompiledAggregate>,
    schema: Arc<Schema>,
    /// Key to index into `group_keys` / `accumulators`.
    table: HashMap<Vec<KeyValue>, usize>,
    /// Insertion-ordered, so output is deterministic.
    group_keys: Vec<Vec<ScalarValue>>,
    accumulators: Vec<Vec<Accumulator>>,
    built: bool,
    emitted: usize,
    stats: OperatorStats,
}

impl HashAggregateExec {
    pub fn new(
        input: Box<dyn Operator>,
        group_exprs: Vec<CompiledExpr>,
        aggregates: Vec<CompiledAggregate>,
        schema: Arc<Schema>,
    ) -> HashAggregateExec {
        let detail = if group_exprs.is_empty() {
            format!("hash aggregate, no grouping, {} aggregate(s)", aggregates.len())
        } else {
            format!(
                "hash aggregate on {} key(s), {} aggregate(s)",
                group_exprs.len(),
                aggregates.len()
            )
        };
        HashAggregateExec {
            input,
            group_exprs,
            aggregates,
            schema,
            table: HashMap::new(),
            group_keys: Vec::new(),
            accumulators: Vec::new(),
            built: false,
            emitted: 0,
            stats: OperatorStats::new("HashAggregate", detail),
        }
    }

    fn new_group(&mut self, key: Vec<ScalarValue>) -> usize {
        let index = self.group_keys.len();
        self.group_keys.push(key);
        self.accumulators
            .push(self.aggregates.iter().map(Accumulator::new).collect());
        index
    }

    fn build(&mut self) -> Result<()> {
        if self.built {
            return Ok(());
        }
        self.built = true;

        // A query with no GROUP BY always produces exactly one row, so its
        // group exists before any input is read. With a GROUP BY it does not:
        // no rows means no groups.
        if self.group_exprs.is_empty() {
            let index = self.new_group(Vec::new());
            self.table.insert(Vec::new(), index);
        }

        while let Some(batch) = self.input.next()? {
            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;

            let key_cols: Vec<Arc<Column>> = self
                .group_exprs
                .iter()
                .map(|e| expr::eval(e, &batch))
                .collect::<Result<_>>()?;
            let arg_cols: Vec<Option<Arc<Column>>> = self
                .aggregates
                .iter()
                .map(|a| a.arg.as_ref().map(|e| expr::eval(e, &batch)).transpose())
                .collect::<Result<_>>()?;

            for row in 0..batch.num_rows() {
                let values: Vec<ScalarValue> = key_cols.iter().map(|c| c.value(row)).collect();
                // NULL is a perfectly good group key here -- see the module note.
                let key: Vec<KeyValue> = values.iter().map(KeyValue::of).collect();
                let index = match self.table.get(&key) {
                    Some(i) => *i,
                    None => {
                        let i = self.new_group(values);
                        self.table.insert(key, i);
                        i
                    }
                };
                for (a, col) in self.accumulators[index].iter_mut().zip(&arg_cols) {
                    let value = col.as_ref().map(|c| c.value(row));
                    a.update(value.as_ref())?;
                }
            }
            self.stats.elapsed_nanos += timer.elapsed_nanos();
        }
        self.stats.peak_batch_bytes = self
            .stats
            .peak_batch_bytes
            .max(self.group_keys.len() * (self.group_exprs.len() + self.aggregates.len()) * 16);
        Ok(())
    }
}

impl Operator for HashAggregateExec {

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
        if self.emitted >= self.group_keys.len() {
            return Ok(None);
        }

        let timer = Timer::start();
        let end = (self.emitted + DEFAULT_BATCH_SIZE).min(self.group_keys.len());
        let rows = self.emitted..end;

        let mut columns: Vec<Column> = Vec::with_capacity(self.schema.len());
        for (g, _) in self.group_exprs.iter().enumerate() {
            let mut b = ColumnBuilder::new(&self.schema.field(g).data_type);
            for i in rows.clone() {
                b.append(&self.group_keys[i][g])?;
            }
            columns.push(b.finish());
        }
        for (a, agg) in self.aggregates.iter().enumerate() {
            let mut b = ColumnBuilder::new(&agg.data_type);
            for i in rows.clone() {
                b.append(&self.accumulators[i][a].finish(agg.data_type))?;
            }
            columns.push(b.finish());
        }
        self.emitted = end;

        let out = Batch::dense(Arc::clone(&self.schema), columns);
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
// Sort-based aggregation
// ---------------------------------------------------------------------------

/// Aggregation over an input whose groups already arrive together.
///
/// Named for what it does rather than for what it needs: it streams. Sorted
/// input is the usual way its groups arrive contiguously, but an aggregate with
/// no GROUP BY has a single group and needs no ordering at all -- and that case
/// is better served here than by a hash table of one entry.
///
/// The other half of the aggregate choice. A hash aggregate holds one
/// accumulator set per distinct group for the whole query; a streaming one
/// holds exactly one, because sorted input puts every group's rows together and
/// a group is finished the moment the key changes. Memory goes from
/// O(distinct groups) to O(1).
///
/// It also emits in group-key order, which a hash aggregate cannot promise --
/// so an `ORDER BY` on the grouping columns above it is free.
///
/// The precondition is the input's ordering, which `exec::ordering` establishes
/// and the planner checks. Given unsorted input this operator does not fail; it
/// silently produces one group per *run*, which is why nothing may construct it
/// without that check.
pub struct SortAggregateExec {
    input: Box<dyn Operator>,
    group_exprs: Vec<CompiledExpr>,
    aggregates: Vec<CompiledAggregate>,
    schema: Arc<Schema>,
    /// The group being accumulated: its key values and its accumulators.
    open: Option<(Vec<ScalarValue>, Vec<Accumulator>)>,
    /// Groups finished but not yet handed upward.
    ready: Vec<(Vec<ScalarValue>, Vec<Accumulator>)>,
    finished: bool,
    stats: OperatorStats,
}

impl SortAggregateExec {
    pub fn new(
        input: Box<dyn Operator>,
        group_exprs: Vec<CompiledExpr>,
        aggregates: Vec<CompiledAggregate>,
        schema: Arc<Schema>,
    ) -> SortAggregateExec {
        let detail = if group_exprs.is_empty() {
            "streaming aggregate, no grouping".to_string()
        } else {
            format!(
                "streaming aggregate on {} sorted key(s), {} aggregate(s)",
                group_exprs.len(),
                aggregates.len()
            )
        };
        SortAggregateExec {
            input,
            group_exprs,
            aggregates,
            schema,
            open: None,
            ready: Vec::new(),
            finished: false,
            stats: OperatorStats::new("StreamAggregate", detail),
        }
    }

    fn new_accumulators(&self) -> Vec<Accumulator> {
        self.aggregates.iter().map(Accumulator::new).collect()
    }

    /// Consume input until `target` groups are complete, or the input ends.
    ///
    /// Bounded by a count rather than by "at least one" so the caller can loop
    /// on the same condition: a `while ready.len() < N` outside and a
    /// `while ready.is_empty()` inside make no progress once one group is
    /// ready, and spin.
    fn fill(&mut self, target: usize) -> Result<()> {
        while self.ready.len() < target && !self.finished {
            let Some(batch) = self.input.next()? else {
                self.finished = true;
                // A query with no GROUP BY produces exactly one row even over
                // empty input -- COUNT returns 0 and SUM returns NULL. With a
                // GROUP BY, no rows means no groups.
                if let Some(group) = self.open.take() {
                    self.ready.push(group);
                } else if self.group_exprs.is_empty() {
                    self.ready.push((Vec::new(), self.new_accumulators()));
                }
                break;
            };

            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;

            let key_cols: Vec<Arc<Column>> = self
                .group_exprs
                .iter()
                .map(|e| expr::eval(e, &batch))
                .collect::<Result<_>>()?;
            let arg_cols: Vec<Option<Arc<Column>>> = self
                .aggregates
                .iter()
                .map(|a| a.arg.as_ref().map(|e| expr::eval(e, &batch)).transpose())
                .collect::<Result<_>>()?;

            for row in 0..batch.num_rows() {
                let values: Vec<ScalarValue> = key_cols.iter().map(|c| c.value(row)).collect();
                // NULL groups with NULL here, exactly as in the hash aggregate:
                // grouping treats NULLs as equal even though comparison does
                // not. The sort puts them together, so the run is contiguous.
                let same = match &self.open {
                    Some((key, _)) => keys_equal(key, &values),
                    None => false,
                };
                if !same {
                    if let Some(group) = self.open.take() {
                        self.ready.push(group);
                    }
                    self.open = Some((values, self.new_accumulators()));
                }
                let (_, accs) = self.open.as_mut().expect("a group is open");
                for (a, col) in accs.iter_mut().zip(&arg_cols) {
                    let value = col.as_ref().map(|c| c.value(row));
                    a.update(value.as_ref())?;
                }
            }
            self.stats.elapsed_nanos += timer.elapsed_nanos();
        }
        Ok(())
    }
}

/// Group-key equality: NULL equals NULL, which is what grouping means and what
/// comparison does not.
fn keys_equal(a: &[ScalarValue], b: &[ScalarValue]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| KeyValue::of(x) == KeyValue::of(y))
}

impl Operator for SortAggregateExec {
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
        self.fill(DEFAULT_BATCH_SIZE)?;
        if self.ready.is_empty() {
            return Ok(None);
        }

        let timer = Timer::start();
        let take = self.ready.len().min(DEFAULT_BATCH_SIZE);
        let groups: Vec<(Vec<ScalarValue>, Vec<Accumulator>)> =
            self.ready.drain(..take).collect();

        let mut columns: Vec<Column> = Vec::with_capacity(self.schema.len());
        for g in 0..self.group_exprs.len() {
            let mut b = ColumnBuilder::new(&self.schema.field(g).data_type);
            for (key, _) in &groups {
                b.append(&key[g])?;
            }
            columns.push(b.finish());
        }
        for (a, agg) in self.aggregates.iter().enumerate() {
            let mut b = ColumnBuilder::new(&agg.data_type);
            for (_, accs) in &groups {
                b.append(&accs[a].finish(agg.data_type))?;
            }
            columns.push(b.finish());
        }

        let out = Batch::dense(Arc::clone(&self.schema), columns);
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
