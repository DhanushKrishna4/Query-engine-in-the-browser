//! `DISTINCT` and the set operations.
//!
//! All four share one idea: a row's identity is the tuple of its values, and
//! two rows are the same when those tuples match. That is not the same
//! comparison SQL's `=` uses -- **NULL counts as equal to NULL here**, exactly
//! as it does for `GROUP BY` and exactly as it does not for a join key. Set
//! operations ask "is this the same row?", not "are these values equal?".
//!
//! `UNION ALL` is the odd one out: it never has to identify anything, so it
//! streams both inputs straight through.
//!
//! `INTERSECT` and `EXCEPT` default to removing duplicates; `ALL` makes them
//! multiset operations, keeping `min(left, right)` and `max(0, left - right)`
//! copies respectively. That is why the right side is counted rather than
//! merely collected into a set.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::exec::key::KeyValue;
use crate::exec::{Operator, OperatorStats, Timer};
use crate::plan::SetOperator;
use crate::storage::{Batch, Schema};

/// The identity of one row: every column value, in order.
fn row_key(batch: &Batch, row: usize) -> Vec<KeyValue> {
    (0..batch.columns().len())
        .map(|c| KeyValue::of(&batch.value(c, row)))
        .collect()
}

// ---------------------------------------------------------------------------
// DISTINCT
// ---------------------------------------------------------------------------

/// Emit the first occurrence of each distinct row.
///
/// Streaming: a row is emitted as soon as it is seen to be new, so a `DISTINCT`
/// under a `LIMIT` does not have to read the whole input. Memory is bounded by
/// the number of *distinct* rows, not the input.
pub struct DistinctExec {
    input: Box<dyn Operator>,
    seen: std::collections::HashSet<Vec<KeyValue>>,
    stats: OperatorStats,
}

impl DistinctExec {
    pub fn new(input: Box<dyn Operator>) -> DistinctExec {
        DistinctExec {
            input,
            seen: std::collections::HashSet::new(),
            stats: OperatorStats::new("Distinct", "hash distinct".to_string()),
        }
    }
}

impl Operator for DistinctExec {
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
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        loop {
            let Some(batch) = self.input.next()? else {
                return Ok(None);
            };
            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;

            let keep: Vec<u32> = (0..batch.num_rows())
                .filter(|row| self.seen.insert(row_key(&batch, *row)))
                .map(|row| row as u32)
                .collect();
            let out = batch.filter(&keep);
            self.stats.elapsed_nanos += timer.elapsed_nanos();

            if out.num_rows() == 0 {
                continue;
            }
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}

// ---------------------------------------------------------------------------
// UNION / INTERSECT / EXCEPT
// ---------------------------------------------------------------------------

pub struct SetOpExec {
    left: Box<dyn Operator>,
    right: Box<dyn Operator>,
    op: SetOperator,
    all: bool,
    schema: Arc<Schema>,
    /// For INTERSECT and EXCEPT: how many times each row appears on the right.
    counts: Option<HashMap<Vec<KeyValue>, usize>>,
    /// For the DISTINCT forms: rows already emitted.
    emitted: std::collections::HashSet<Vec<KeyValue>>,
    /// UNION ALL only: whether the left input is exhausted.
    left_done: bool,
    stats: OperatorStats,
}

impl SetOpExec {
    pub fn new(
        left: Box<dyn Operator>,
        right: Box<dyn Operator>,
        op: SetOperator,
        all: bool,
        schema: Arc<Schema>,
    ) -> SetOpExec {
        let detail = format!("{}{}", op.as_str(), if all { " ALL" } else { "" });
        SetOpExec {
            left,
            right,
            op,
            all,
            schema,
            counts: None,
            emitted: std::collections::HashSet::new(),
            left_done: false,
            stats: OperatorStats::new("SetOp", detail),
        }
    }

    /// Count the right input. Only INTERSECT and EXCEPT need it; UNION reads
    /// the right side as rows rather than as a set.
    fn ensure_counts(&mut self) -> Result<()> {
        if self.counts.is_some() || self.op == SetOperator::Union {
            return Ok(());
        }
        let mut counts: HashMap<Vec<KeyValue>, usize> = HashMap::new();
        while let Some(batch) = self.right.next()? {
            self.stats.rows_in += batch.num_rows() as u64;
            for row in 0..batch.num_rows() {
                *counts.entry(row_key(&batch, row)).or_insert(0) += 1;
            }
        }
        self.counts = Some(counts);
        Ok(())
    }

    /// Which rows of a left-side batch survive.
    fn keep(&mut self, batch: &Batch) -> Vec<u32> {
        let mut keep = Vec::new();
        for row in 0..batch.num_rows() {
            let key = row_key(batch, row);
            let survives = match (self.op, self.all) {
                (SetOperator::Union, true) => true,
                (SetOperator::Union, false) => self.emitted.insert(key),

                (SetOperator::Intersect, true) => {
                    // Multiset: one output row per matching pair, so the right
                    // side's count is consumed.
                    match self.counts.as_mut().and_then(|c| c.get_mut(&key)) {
                        Some(n) if *n > 0 => {
                            *n -= 1;
                            true
                        }
                        _ => false,
                    }
                }
                (SetOperator::Intersect, false) => {
                    let present = self
                        .counts
                        .as_ref()
                        .is_some_and(|c| c.get(&key).is_some_and(|n| *n > 0));
                    present && self.emitted.insert(key)
                }

                (SetOperator::Except, true) => {
                    // Each right-side copy cancels one left-side copy.
                    match self.counts.as_mut().and_then(|c| c.get_mut(&key)) {
                        Some(n) if *n > 0 => {
                            *n -= 1;
                            false
                        }
                        _ => true,
                    }
                }
                (SetOperator::Except, false) => {
                    let present = self
                        .counts
                        .as_ref()
                        .is_some_and(|c| c.get(&key).is_some_and(|n| *n > 0));
                    !present && self.emitted.insert(key)
                }
            };
            if survives {
                keep.push(row as u32);
            }
        }
        keep
    }
}

impl Operator for SetOpExec {
    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.left.as_mut()),
            1 => Some(self.right.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        self.ensure_counts()?;

        loop {
            // UNION reads both inputs as rows; the others have already consumed
            // the right side into counts.
            let next = if !self.left_done {
                match self.left.next()? {
                    Some(b) => Some(b),
                    None => {
                        self.left_done = true;
                        continue;
                    }
                }
            } else if self.op == SetOperator::Union {
                self.right.next()?
            } else {
                None
            };

            let Some(batch) = next else {
                return Ok(None);
            };

            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;
            let keep = self.keep(&batch);
            let out = batch.filter(&keep);
            self.stats.elapsed_nanos += timer.elapsed_nanos();

            if out.num_rows() == 0 {
                continue;
            }
            // The two branches were coerced to one schema by the binder, but
            // each still carries its own; the output takes the set's.
            let out = Batch::new(
                Arc::clone(&self.schema),
                out.columns().to_vec(),
                out.selection().clone(),
            );
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }
}
