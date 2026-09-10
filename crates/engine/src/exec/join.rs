//! Join operators.
//!
//! Two implementations, and the choice between them is the engine's first real
//! operator selection: if the condition contains at least one equality between
//! the two sides, a hash join can use it; otherwise there is nothing to hash on
//! and the only option is to compare every pair.
//!
//! Both build the right input fully into memory and stream the left through it.
//! Which side *should* be built is a cost question -- you want the smaller one --
//! and answering it needs cardinality estimates, so for now the right input is
//! built and the choice is recorded in the operator's stats as unexplained.
//!
//! ## Semi and anti joins
//!
//! `EXISTS` and `IN` become semi-joins, `NOT EXISTS` an anti-join. These are
//! filters wearing a join's clothes: they emit each *left* row at most once and
//! contribute no columns of their own. The right side is still built and probed
//! the same way; only what comes out is different.
//!
//! The residual condition still has to be evaluated against a full joined row,
//! so the operator carries two schemas -- the one it emits and the wider one it
//! evaluates against.
//!
//! ## NULLs
//!
//! An equi-join must never match NULL against NULL: `NULL = NULL` is unknown,
//! not true. So rows whose key contains a NULL are left out of the hash table
//! and find nothing when they probe -- and then, because they are still rows,
//! an outer join pads and emits them anyway. NaN is treated the same way for
//! the same reason.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::exec::key::{unmatchable, KeyValue};
use crate::exec::{OperatorStats, Timer};
use crate::expr::{self, CompiledExpr};
use crate::plan::JoinType;
use crate::storage::{Batch, Column, ColumnBuilder, Schema};

use super::Operator;

/// The right input, fully materialized into dense columns so that any row can
/// be gathered by index.
struct BuildSide {
    columns: Vec<Column>,
    num_rows: usize,
    /// Which build rows found a match. Only tracked for join types that have to
    /// emit the ones that did not.
    matched: Vec<bool>,
    index: HashMap<Vec<KeyValue>, Vec<u32>>,
}

impl BuildSide {
    fn collect(
        input: &mut dyn Operator,
        keys: &[CompiledExpr],
        track_matches: bool,
    ) -> Result<BuildSide> {
        let schema = input.schema();
        let mut builders: Vec<ColumnBuilder> = schema
            .fields
            .iter()
            .map(|f| ColumnBuilder::new(&f.data_type))
            .collect();
        let mut key_rows: Vec<Option<Vec<KeyValue>>> = Vec::new();

        while let Some(batch) = input.next()? {
            let key_cols: Vec<Arc<Column>> = keys
                .iter()
                .map(|k| expr::eval(k, &batch))
                .collect::<Result<_>>()?;

            for row in 0..batch.num_rows() {
                for (c, b) in builders.iter_mut().enumerate() {
                    b.append(&batch.value(c, row))?;
                }
                key_rows.push(row_key(&key_cols, row));
            }
        }

        let columns: Vec<Column> = builders.into_iter().map(|b| b.finish()).collect();
        let num_rows = key_rows.len();

        let mut index: HashMap<Vec<KeyValue>, Vec<u32>> = HashMap::new();
        if !keys.is_empty() {
            for (i, k) in key_rows.iter().enumerate() {
                if let Some(k) = k {
                    index.entry(k.clone()).or_default().push(i as u32);
                }
            }
        }

        Ok(BuildSide {
            columns,
            num_rows,
            matched: if track_matches {
                vec![false; num_rows]
            } else {
                Vec::new()
            },
            index,
        })
    }

    fn bytes(&self) -> usize {
        self.columns.iter().map(|c| c.byte_size()).sum()
    }
}

/// The key of one row, or `None` if it can never match.
fn row_key(key_cols: &[Arc<Column>], row: usize) -> Option<Vec<KeyValue>> {
    let mut out = Vec::with_capacity(key_cols.len());
    fill_key(key_cols, row, &mut out).then_some(out)
}

/// The same, into a caller-owned buffer. The probe side calls this once per
/// row, so reusing the vector keeps an allocation out of the inner loop.
///
/// The `String` inside a text key is still allocated per row. Removing that
/// means keying the table on a hash and verifying candidates against the stored
/// build keys, which is worth doing once joins are hot enough to matter --
/// today the dominant cost is materializing output rows the filter above will
/// immediately discard, and predicate pushdown fixes that instead.
fn fill_key(key_cols: &[Arc<Column>], row: usize, out: &mut Vec<KeyValue>) -> bool {
    out.clear();
    for c in key_cols {
        let v = c.value(row);
        if unmatchable(&v) {
            return false;
        }
        out.push(KeyValue::of(&v));
    }
    true
}

/// One output row: which probe row and which build row it came from. `None` on
/// either side means that side is NULL-padded.
type Pair = (Option<usize>, Option<usize>);

/// Materialize output rows from probe and build rows.
fn emit(
    schema: &Arc<Schema>,
    probe: Option<&Batch>,
    build: &BuildSide,
    pairs: &[Pair],
    include_build: bool,
) -> Batch {
    let left_width = if include_build {
        schema.len() - build.columns.len()
    } else {
        schema.len()
    };
    let mut columns = Vec::with_capacity(schema.len());

    let probe_rows: Vec<Option<usize>> = pairs
        .iter()
        .map(|(p, _)| p.map(|logical| probe.expect("probe rows need a batch").selection().physical(logical)))
        .collect();
    for c in 0..left_width {
        columns.push(match probe {
            Some(b) => b.column(c).take_opt(&probe_rows),
            // A right-outer flush has no probe batch, so every left column is
            // entirely NULL.
            None => {
                let mut builder = ColumnBuilder::new(&schema.field(c).data_type);
                for _ in pairs {
                    builder.append_null();
                }
                builder.finish()
            }
        });
    }

    if include_build {
        let build_rows: Vec<Option<usize>> = pairs.iter().map(|(_, b)| *b).collect();
        for c in &build.columns {
            columns.push(c.take_opt(&build_rows));
        }
    }

    // The row count is stated rather than inferred from the columns, because
    // projection pushdown can leave a join with none at all -- `SELECT COUNT(*)
    // FROM a CROSS JOIN b` reads no column of either side, and the rows still
    // have to be counted.
    Batch::new(
        Arc::clone(schema),
        columns.into_iter().map(Arc::new).collect(),
        crate::storage::Selection::full(pairs.len()),
    )
}

/// Turn surviving candidate pairs into the rows this join type emits.
///
/// An ordinary join emits every surviving pair, plus a NULL-padded row for each
/// unmatched probe row when it preserves the left side. A semi or anti join
/// emits each probe row at most *once* -- which is the whole point: `EXISTS`
/// asks whether a match exists, not how many there are, so a left row with
/// three matches must not become three rows.
fn output_pairs(join_type: JoinType, kept: Vec<Pair>, probe_matched: &[bool]) -> Vec<Pair> {
    match join_type {
        JoinType::Semi => probe_matched
            .iter()
            .enumerate()
            .filter(|(_, matched)| **matched)
            .map(|(row, _)| (Some(row), None))
            .collect(),
        JoinType::Anti => probe_matched
            .iter()
            .enumerate()
            .filter(|(_, matched)| !**matched)
            .map(|(row, _)| (Some(row), None))
            .collect(),
        _ => {
            let mut pairs = kept;
            if join_type.preserves_left() {
                for (row, matched) in probe_matched.iter().enumerate() {
                    if !matched {
                        pairs.push((Some(row), None));
                    }
                }
            }
            pairs
        }
    }
}

/// Apply the non-equi part of the condition to a candidate batch, returning the
/// pairs that survive.
fn apply_residual(
    residual: Option<&CompiledExpr>,
    schema: &Arc<Schema>,
    probe: Option<&Batch>,
    build: &BuildSide,
    pairs: Vec<Pair>,
) -> Result<Vec<Pair>> {
    let Some(residual) = residual else {
        return Ok(pairs);
    };
    if pairs.is_empty() {
        return Ok(pairs);
    }
    // The residual reads columns from both sides, so it is evaluated against
    // the full joined row even when the join will only emit the left one.
    let candidate = emit(schema, probe, build, &pairs, true);
    let keep = expr::eval_predicate(residual, &candidate)?;
    Ok(keep.into_iter().map(|i| pairs[i as usize]).collect())
}

// ---------------------------------------------------------------------------
// Hash join
// ---------------------------------------------------------------------------

pub struct HashJoinExec {
    probe: Box<dyn Operator>,
    build_input: Option<Box<dyn Operator>>,
    build: Option<BuildSide>,
    join_type: JoinType,
    probe_keys: Vec<CompiledExpr>,
    build_keys: Vec<CompiledExpr>,
    /// Conditions that are not equalities between the two sides. Evaluated
    /// against the joined row, after candidates have been paired up.
    residual: Option<CompiledExpr>,
    schema: Arc<Schema>,
    /// Left columns followed by right columns. Equal to `schema` for an
    /// ordinary join; wider for a semi or anti join, which emits only the left
    /// side but must still evaluate its condition against both.
    candidate_schema: Arc<Schema>,
    /// Set once the probe side is exhausted and only unmatched build rows are
    /// left to emit.
    flushing: bool,
    flush_pos: usize,
    stats: OperatorStats,
}

impl HashJoinExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        probe: Box<dyn Operator>,
        build_input: Box<dyn Operator>,
        join_type: JoinType,
        probe_keys: Vec<CompiledExpr>,
        build_keys: Vec<CompiledExpr>,
        residual: Option<CompiledExpr>,
        schema: Arc<Schema>,
        candidate_schema: Arc<Schema>,
    ) -> HashJoinExec {
        let detail = format!(
            "{} hash join on {} equi-key(s){}",
            join_type.as_str(),
            probe_keys.len(),
            match &residual {
                Some(r) => format!(", residual {}", r.span.start),
                None => String::new(),
            }
        );
        HashJoinExec {
            probe,
            build_input: Some(build_input),
            build: None,
            join_type,
            probe_keys,
            build_keys,
            residual,
            schema,
            candidate_schema,
            flushing: false,
            flush_pos: 0,
            stats: OperatorStats::new("HashJoin", detail),
        }
    }

    fn ensure_built(&mut self) -> Result<()> {
        if self.build.is_some() {
            return Ok(());
        }
        let mut input = self.build_input.take().expect("build runs once");
        let timer = Timer::start();
        let build = BuildSide::collect(
            input.as_mut(),
            &self.build_keys,
            self.join_type.preserves_right(),
        )?;
        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.stats.rows_in += build.num_rows as u64;
        self.stats.peak_batch_bytes = self.stats.peak_batch_bytes.max(build.bytes());
        self.build_input = Some(input);
        self.build = Some(build);
        Ok(())
    }
}

impl Operator for HashJoinExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn set_reason(&mut self, reason: String) {
        self.stats.because(reason);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.probe.as_mut()),
            1 => match &mut self.build_input {
                Some(b) => Some(b.as_mut()),
                None => None,
            },
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        self.ensure_built()?;

        loop {
            if self.flushing {
                let timer = Timer::start();
                let build = self.build.as_ref().expect("built above");
                let mut pairs: Vec<Pair> = Vec::new();
                let mut pos = self.flush_pos;
                while pos < build.num_rows && pairs.len() < crate::storage::DEFAULT_BATCH_SIZE {
                    if !build.matched[pos] {
                        pairs.push((None, Some(pos)));
                    }
                    pos += 1;
                }
                self.flush_pos = pos;
                if pairs.is_empty() {
                    self.stats.elapsed_nanos += timer.elapsed_nanos();
                    return Ok(None);
                }
                let out = emit(&self.schema, None, build, &pairs, true);
                self.stats.elapsed_nanos += timer.elapsed_nanos();
                self.stats.record_output(&out);
                return Ok(Some(out));
            }

            let Some(batch) = self.probe.next()? else {
                if self.join_type.preserves_right() {
                    self.flushing = true;
                    continue;
                }
                return Ok(None);
            };

            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;

            let key_cols: Vec<Arc<Column>> = self
                .probe_keys
                .iter()
                .map(|k| expr::eval(k, &batch))
                .collect::<Result<_>>()?;

            // Candidate pairs first, then the residual, then unmatched probe
            // rows. Doing it in that order is what makes an outer join with a
            // residual correct: a probe row whose only candidate fails the
            // residual still has to be emitted, NULL-padded.
            let mut pairs: Vec<Pair> = Vec::new();
            let build = self.build.as_ref().expect("built above");
            let mut key = Vec::with_capacity(self.probe_keys.len());
            for row in 0..batch.num_rows() {
                if !fill_key(&key_cols, row, &mut key) {
                    continue;
                }
                if let Some(matches) = build.index.get(&key) {
                    for m in matches {
                        pairs.push((Some(row), Some(*m as usize)));
                    }
                }
            }

            let kept = apply_residual(
                self.residual.as_ref(),
                &self.candidate_schema,
                Some(&batch),
                build,
                pairs,
            )?;

            let mut probe_matched = vec![false; batch.num_rows()];
            for (p, b) in &kept {
                if let Some(p) = p {
                    probe_matched[*p] = true;
                }
                if let (Some(b), true) = (b, self.join_type.preserves_right()) {
                    self.build
                        .as_mut()
                        .expect("built above")
                        .matched[*b] = true;
                }
            }

            let pairs = output_pairs(self.join_type, kept, &probe_matched);

            if pairs.is_empty() {
                self.stats.elapsed_nanos += timer.elapsed_nanos();
                continue;
            }
            let build = self.build.as_ref().expect("built above");
            let out = emit(
                &self.schema,
                Some(&batch),
                build,
                &pairs,
                !self.join_type.is_filtering(),
            );
            self.stats.elapsed_nanos += timer.elapsed_nanos();
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        match &self.build_input {
            Some(b) => vec![self.probe.as_ref(), b.as_ref()],
            None => vec![self.probe.as_ref()],
        }
    }
}

// ---------------------------------------------------------------------------
// Nested loop join
// ---------------------------------------------------------------------------

/// Every probe row against every build row.
///
/// Used when there is no equality to hash on: a cross join, or a condition like
/// `a.x < b.y`. Quadratic, and honestly so -- the alternative is not a cleverer
/// nested loop but a different join condition.
pub struct NestedLoopJoinExec {
    probe: Box<dyn Operator>,
    build_input: Option<Box<dyn Operator>>,
    build: Option<BuildSide>,
    join_type: JoinType,
    condition: Option<CompiledExpr>,
    schema: Arc<Schema>,
    candidate_schema: Arc<Schema>,
    flushing: bool,
    flush_pos: usize,
    stats: OperatorStats,
}

impl NestedLoopJoinExec {
    pub fn new(
        probe: Box<dyn Operator>,
        build_input: Box<dyn Operator>,
        join_type: JoinType,
        condition: Option<CompiledExpr>,
        schema: Arc<Schema>,
        candidate_schema: Arc<Schema>,
    ) -> NestedLoopJoinExec {
        let detail = format!(
            "{} nested loop join ({})",
            join_type.as_str(),
            if condition.is_some() {
                "no equi-key to hash on"
            } else {
                "no condition"
            }
        );
        NestedLoopJoinExec {
            probe,
            build_input: Some(build_input),
            build: None,
            join_type,
            condition,
            schema,
            candidate_schema,
            flushing: false,
            flush_pos: 0,
            stats: OperatorStats::new("NestedLoopJoin", detail),
        }
    }
}

impl Operator for NestedLoopJoinExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn set_reason(&mut self, reason: String) {
        self.stats.because(reason);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.probe.as_mut()),
            1 => match &mut self.build_input {
                Some(b) => Some(b.as_mut()),
                None => None,
            },
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        if self.build.is_none() {
            let mut input = self.build_input.take().expect("build runs once");
            let timer = Timer::start();
            let build =
                BuildSide::collect(input.as_mut(), &[], self.join_type.preserves_right())?;
            self.stats.elapsed_nanos += timer.elapsed_nanos();
            self.stats.rows_in += build.num_rows as u64;
            self.build_input = Some(input);
            self.build = Some(build);
        }

        loop {
            if self.flushing {
                let timer = Timer::start();
                let build = self.build.as_ref().expect("built above");
                let mut pairs: Vec<Pair> = Vec::new();
                let mut pos = self.flush_pos;
                while pos < build.num_rows && pairs.len() < crate::storage::DEFAULT_BATCH_SIZE {
                    if !build.matched[pos] {
                        pairs.push((None, Some(pos)));
                    }
                    pos += 1;
                }
                self.flush_pos = pos;
                if pairs.is_empty() {
                    self.stats.elapsed_nanos += timer.elapsed_nanos();
                    return Ok(None);
                }
                let out = emit(&self.schema, None, build, &pairs, true);
                self.stats.elapsed_nanos += timer.elapsed_nanos();
                self.stats.record_output(&out);
                return Ok(Some(out));
            }

            let Some(batch) = self.probe.next()? else {
                if self.join_type.preserves_right() {
                    self.flushing = true;
                    continue;
                }
                return Ok(None);
            };

            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;
            let build = self.build.as_ref().expect("built above");

            let mut pairs: Vec<Pair> = Vec::with_capacity(batch.num_rows() * build.num_rows);
            for row in 0..batch.num_rows() {
                for b in 0..build.num_rows {
                    pairs.push((Some(row), Some(b)));
                }
            }

            let kept = apply_residual(
                self.condition.as_ref(),
                &self.candidate_schema,
                Some(&batch),
                build,
                pairs,
            )?;

            let mut probe_matched = vec![false; batch.num_rows()];
            for (p, b) in &kept {
                if let Some(p) = p {
                    probe_matched[*p] = true;
                }
                if let (Some(b), true) = (b, self.join_type.preserves_right()) {
                    self.build.as_mut().expect("built above").matched[*b] = true;
                }
            }

            let pairs = output_pairs(self.join_type, kept, &probe_matched);

            if pairs.is_empty() {
                self.stats.elapsed_nanos += timer.elapsed_nanos();
                continue;
            }
            let build = self.build.as_ref().expect("built above");
            let out = emit(
                &self.schema,
                Some(&batch),
                build,
                &pairs,
                !self.join_type.is_filtering(),
            );
            self.stats.elapsed_nanos += timer.elapsed_nanos();
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        match &self.build_input {
            Some(b) => vec![self.probe.as_ref(), b.as_ref()],
            None => vec![self.probe.as_ref()],
        }
    }
}
