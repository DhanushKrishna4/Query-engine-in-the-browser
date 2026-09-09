//! Merge join: the third join algorithm, for inputs that arrive sorted.
//!
//! A hash join builds a table over one whole side before it can probe. A nested
//! loop compares every pair. A merge join does neither: it walks both inputs
//! once, in lockstep, and never looks backward. Its memory is one *run* -- the
//! group of rows sharing a key -- rather than a whole side, and its output
//! arrives sorted on the join key, which can make an `ORDER BY` above it free.
//!
//! The price is the precondition: both inputs must already be sorted on the
//! join keys, in the same direction, with NULLs in the same place. That is what
//! `exec::ordering` exists to establish, and why this operator could not be
//! written before it.
//!
//! ## Runs, not rows
//!
//! The whole difficulty is duplicate keys. If the left has three rows with key
//! 7 and the right has two, the answer is all six pairs -- so the right's run
//! must be buffered and replayed for each left row that shares the key. Only
//! the *right* run is buffered: each left row is handled independently against
//! it, so the left side stays a cursor.
//!
//! ## NULLs never match
//!
//! A join key of NULL joins to nothing, on either side, exactly as it does in a
//! hash join -- `NULL = NULL` is unknown, not true. But NULLs still occupy a
//! position in the sort, so they cannot simply be skipped: they have to be
//! walked past in key order, and for an outer join they are unmatched rows that
//! still need emitting. Treating them as ordinary values that happen to match
//! nothing is what keeps the merge in step.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::error::Result;
use crate::expr::{self, CompiledExpr};
use crate::plan::JoinType;
use crate::storage::{Batch, ColumnBuilder, Schema, DEFAULT_BATCH_SIZE};
use crate::types::{self, ScalarValue};

use super::{Operator, OperatorStats, Timer};

/// One side's cursor: the batch it is reading, where it is in it, and the key
/// expressions to evaluate.
struct Side {
    input: Box<dyn Operator>,
    keys: Vec<CompiledExpr>,
    batch: Option<Batch>,
    row: usize,
    done: bool,
}

impl Side {
    fn new(input: Box<dyn Operator>, keys: Vec<CompiledExpr>) -> Side {
        Side {
            input,
            keys,
            batch: None,
            row: 0,
            done: false,
        }
    }

    /// Make sure a row is available, pulling batches until one is or the input
    /// is exhausted. Empty batches are skipped rather than trusted.
    fn fill(&mut self) -> Result<bool> {
        loop {
            if let Some(b) = &self.batch {
                if self.row < b.num_rows() {
                    return Ok(true);
                }
            }
            if self.done {
                return Ok(false);
            }
            match self.input.next()? {
                Some(b) => {
                    self.batch = Some(b);
                    self.row = 0;
                }
                None => {
                    self.done = true;
                    self.batch = None;
                    return Ok(false);
                }
            }
        }
    }

    /// The current row's key values, or `None` at end of input.
    fn key(&mut self) -> Result<Option<Vec<ScalarValue>>> {
        if !self.fill()? {
            return Ok(None);
        }
        let batch = self.batch.as_ref().expect("fill guarantees a batch");
        let row = self.row;
        let mut out = Vec::with_capacity(self.keys.len());
        for k in &self.keys {
            out.push(expr::eval(k, batch)?.value(row));
        }
        Ok(Some(out))
    }

    /// The current row's values across every column.
    fn row_values(&mut self) -> Result<Option<Vec<ScalarValue>>> {
        if !self.fill()? {
            return Ok(None);
        }
        let batch = self.batch.as_ref().expect("fill guarantees a batch");
        Ok(Some(
            (0..batch.columns().len())
                .map(|c| batch.value(c, self.row))
                .collect(),
        ))
    }

    fn advance(&mut self) {
        self.row += 1;
    }
}

pub struct MergeJoinExec {
    left: Side,
    right: Side,
    join_type: JoinType,
    /// Applied to a candidate pair after the keys matched, for the parts of the
    /// condition that were not equalities.
    residual: Option<CompiledExpr>,
    schema: Arc<Schema>,
    /// Left and right columns concatenated, which is what the residual reads
    /// even for a semi join that emits the left alone.
    candidate_schema: Arc<Schema>,
    left_width: usize,
    right_width: usize,

    /// The right rows sharing the key currently being matched.
    run: Vec<Vec<ScalarValue>>,
    /// Which of them have matched something, for RIGHT and FULL joins.
    run_matched: Vec<bool>,
    run_key: Option<Vec<ScalarValue>>,

    pending: Vec<Vec<ScalarValue>>,
    finished: bool,
    stats: OperatorStats,
}

impl MergeJoinExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left: Box<dyn Operator>,
        right: Box<dyn Operator>,
        join_type: JoinType,
        left_keys: Vec<CompiledExpr>,
        right_keys: Vec<CompiledExpr>,
        residual: Option<CompiledExpr>,
        schema: Arc<Schema>,
        candidate_schema: Arc<Schema>,
    ) -> MergeJoinExec {
        let left_width = left.schema().len();
        let right_width = right.schema().len();
        let mut stats = OperatorStats::new(
            "MergeJoin",
            format!(
                "{} merge join on {} key(s), both inputs already sorted",
                join_type.as_str(),
                left_keys.len()
            ),
        );
        stats.detail = match &residual {
            Some(_) => format!("{}, with a residual condition", stats.detail),
            None => stats.detail,
        };
        MergeJoinExec {
            left: Side::new(left, left_keys),
            right: Side::new(right, right_keys),
            join_type,
            residual,
            schema,
            candidate_schema,
            left_width,
            right_width,
            run: Vec::new(),
            run_matched: Vec::new(),
            run_key: None,
            pending: Vec::new(),
            finished: false,
            stats,
        }
    }

    /// Compare two key tuples the way the sort that produced them did.
    ///
    /// NULLs sort together at one end. Which end does not matter for
    /// correctness here as long as both sides agree, and they do: the ordering
    /// property refuses to call two inputs merge-joinable unless their NULL
    /// placement matches.
    fn compare(a: &[ScalarValue], b: &[ScalarValue]) -> Ordering {
        for (x, y) in a.iter().zip(b) {
            let ordering = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => types::compare(x, y).unwrap_or(Ordering::Equal),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// A key that can never match: any NULL in it.
    fn unmatchable(key: &[ScalarValue]) -> bool {
        key.iter().any(|v| v.is_null())
    }

    /// Buffer every right row sharing `key`.
    fn load_run(&mut self, key: &[ScalarValue]) -> Result<()> {
        self.run.clear();
        self.run_matched.clear();
        self.run_key = Some(key.to_vec());
        while let Some(k) = self.right.key()? {
            if Self::compare(&k, key) != Ordering::Equal {
                break;
            }
            let row = self.right.row_values()?.expect("key implies a row");
            self.run.push(row);
            self.run_matched.push(false);
            self.right.advance();
        }
        Ok(())
    }

    /// Emit an unmatched row, padding the other side with NULLs.
    fn emit_outer(&mut self, values: Vec<ScalarValue>, left_side: bool) {
        if self.join_type.is_filtering() {
            // Only an anti join reaches here, and its output is the left row
            // alone -- there are no right columns to pad.
            self.pending.push(values);
            return;
        }
        let mut row = Vec::with_capacity(self.left_width + self.right_width);
        if left_side {
            row.extend(values);
            row.extend(std::iter::repeat_n(ScalarValue::Null, self.right_width));
        } else {
            row.extend(std::iter::repeat_n(ScalarValue::Null, self.left_width));
            row.extend(values);
        }
        self.pending.push(row);
    }

    /// Whether a candidate pair survives the residual condition.
    fn residual_holds(&self, left: &[ScalarValue], right: &[ScalarValue]) -> Result<bool> {
        let Some(residual) = &self.residual else {
            return Ok(true);
        };
        let mut columns = Vec::with_capacity(left.len() + right.len());
        for (i, v) in left.iter().chain(right).enumerate() {
            let mut b = ColumnBuilder::new(&self.candidate_schema.field(i).data_type);
            b.append(v)?;
            columns.push(b.finish());
        }
        let batch = Batch::dense(Arc::clone(&self.candidate_schema), columns);
        // A NULL result is not a match, exactly as in the other joins: the
        // condition has to be TRUE, and UNKNOWN is not TRUE.
        Ok(matches!(
            expr::eval(residual, &batch)?.value(0),
            ScalarValue::Boolean(true)
        ))
    }

    /// Advance the merge until `target` output rows are buffered, or both
    /// inputs are exhausted.
    ///
    /// The bound is a row count rather than "at least one", so that the caller
    /// can loop on the same condition this does. Two loops with different
    /// conditions is how the first version of this spun forever: `next` wanted
    /// a full batch, `step` stopped at one row, and once a row was buffered
    /// neither made progress.
    fn fill(&mut self, target: usize) -> Result<()> {
        while self.pending.len() < target && !self.finished {
            let left_key = self.left.key()?;

            // A run that is still being matched is checked *before* the right
            // cursor, because loading it already moved that cursor past it. A
            // second left row sharing the key has to pair with the buffer, not
            // with whatever the right has advanced to -- comparing against the
            // cursor would call the run finished and drop every duplicate
            // after the first.
            if let (Some(l), Some(run_key)) = (&left_key, &self.run_key) {
                if Self::compare(l, run_key) == Ordering::Equal {
                    let values = self.left.row_values()?.expect("key implies a row");
                    self.left.advance();
                    self.emit_matches(values)?;
                    continue;
                }
            }
            // The left has moved past the run, so anything in it that never
            // matched is now known to be unmatched.
            self.flush_unmatched_run();

            let right_key = self.right.key()?;
            match (left_key, right_key) {
                (None, None) => self.finished = true,

                // One side is exhausted; everything left on the other is
                // unmatched, and only an outer join wants to hear about it.
                (Some(_), None) => {
                    let values = self.left.row_values()?.expect("key implies a row");
                    self.left.advance();
                    if self.join_type.preserves_left() || self.join_type == JoinType::Anti {
                        self.emit_outer(values, true);
                    }
                }
                (None, Some(_)) => {
                    let values = self.right.row_values()?.expect("key implies a row");
                    self.right.advance();
                    if self.join_type.preserves_right() {
                        self.emit_outer(values, false);
                    }
                }

                (Some(l), Some(r)) => {
                    // A NULL key matches nothing on either side, but still
                    // occupies a position in the sort and has to be stepped
                    // over in order rather than skipped.
                    if Self::unmatchable(&l) {
                        let values = self.left.row_values()?.expect("key implies a row");
                        self.left.advance();
                        if self.join_type.preserves_left() || self.join_type == JoinType::Anti {
                            self.emit_outer(values, true);
                        }
                        continue;
                    }
                    if Self::unmatchable(&r) {
                        let values = self.right.row_values()?.expect("key implies a row");
                        self.right.advance();
                        if self.join_type.preserves_right() {
                            self.emit_outer(values, false);
                        }
                        continue;
                    }

                    match Self::compare(&l, &r) {
                        Ordering::Less => {
                            let values = self.left.row_values()?.expect("key implies a row");
                            self.left.advance();
                            if self.join_type.preserves_left()
                                || self.join_type == JoinType::Anti
                            {
                                self.emit_outer(values, true);
                            }
                        }
                        Ordering::Greater => {
                            let values = self.right.row_values()?.expect("key implies a row");
                            self.right.advance();
                            if self.join_type.preserves_right() {
                                self.emit_outer(values, false);
                            }
                        }
                        Ordering::Equal => {
                            self.load_run(&l)?;
                            let values = self.left.row_values()?.expect("key implies a row");
                            self.left.advance();
                            self.emit_matches(values)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Pair one left row against the buffered right run.
    fn emit_matches(&mut self, left: Vec<ScalarValue>) -> Result<()> {
        let mut matched = false;
        for i in 0..self.run.len() {
            if !self.residual_holds(&left, &self.run[i])? {
                continue;
            }
            matched = true;
            self.run_matched[i] = true;
            match self.join_type {
                // A semi join emits the left row once, however many times it
                // matched, and an anti join emits it never.
                JoinType::Semi => {
                    self.pending.push(left.clone());
                    return Ok(());
                }
                JoinType::Anti => return Ok(()),
                _ => {
                    let mut row = Vec::with_capacity(self.left_width + self.right_width);
                    row.extend(left.iter().cloned());
                    row.extend(self.run[i].iter().cloned());
                    self.pending.push(row);
                }
            }
        }
        if !matched {
            match self.join_type {
                JoinType::Anti => self.pending.push(left),
                JoinType::Semi => {}
                _ if self.join_type.preserves_left() => self.emit_outer(left, true),
                _ => {}
            }
        }
        Ok(())
    }

    /// Emit the buffered right rows nothing matched, for RIGHT and FULL joins.
    fn flush_unmatched_run(&mut self) {
        if self.run.is_empty() {
            self.run_key = None;
            return;
        }
        if self.join_type.preserves_right() {
            let unmatched: Vec<Vec<ScalarValue>> = self
                .run
                .iter()
                .zip(&self.run_matched)
                .filter(|(_, m)| !**m)
                .map(|(r, _)| r.clone())
                .collect();
            for row in unmatched {
                self.emit_outer(row, false);
            }
        }
        self.run.clear();
        self.run_matched.clear();
        self.run_key = None;
    }
}

impl Operator for MergeJoinExec {
    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.left.input.as_mut()),
            1 => Some(self.right.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        let timer = Timer::start();
        self.fill(DEFAULT_BATCH_SIZE)?;
        if self.pending.is_empty() {
            self.stats.elapsed_nanos += timer.elapsed_nanos();
            return Ok(None);
        }

        let take = self.pending.len().min(DEFAULT_BATCH_SIZE);
        let rows: Vec<Vec<ScalarValue>> = self.pending.drain(..take).collect();
        let mut columns = Vec::with_capacity(self.schema.len());
        for c in 0..self.schema.len() {
            let mut b = ColumnBuilder::new(&self.schema.field(c).data_type);
            for row in &rows {
                b.append(&row[c])?;
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
        vec![self.left.input.as_ref(), self.right.input.as_ref()]
    }
}
