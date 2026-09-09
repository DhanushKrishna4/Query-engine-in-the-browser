//! Index scans: reaching a few rows through a B+ tree instead of reading every
//! row group that survived pruning.
//!
//! This is the third real operator choice the engine makes, after hash-versus-
//! nested-loop join and sort-versus-top-N. Zone maps and bloom filters can only
//! reject a row group; an index can name the rows. That is worth a great deal
//! for a needle-in-a-haystack predicate and actively harmful for a broad one,
//! because gathering scattered rows gives up the sequential access that makes
//! a columnar scan fast in the first place.
//!
//! ## How the choice is made
//!
//! Not on an estimate. The tree lookup itself reports how many rows match, so
//! the planner asks for the matching rows with a *cap*: if more than
//! [`MAX_INDEX_SELECTIVITY`] of the table qualifies, the tree stops early and
//! says so, and the scan falls back. The abandoned work is bounded by the cap,
//! and in exchange the decision is made on the true row count rather than a
//! histogram's guess.
//!
//! ## What may be served from the index
//!
//! Only `=`, `<`, `<=`, `>`, `>=` and `BETWEEN` against a literal, and only
//! when the literal's type shares a [`value_class`] with the column's. Every
//! one of those predicates is UNKNOWN -- never TRUE -- when the column is NULL,
//! which is exactly the set of rows the tree omits. `IS NULL` is therefore
//! never served here, and `<>` is not either: it would need two ranges, and the
//! union of "everything below" and "everything above" is never selective enough
//! to be worth it.
//!
//! Predicates the extraction does not recognise are simply left to the filter
//! above, which runs regardless. The index narrows *which rows are read*; it
//! never decides which rows are returned.

use std::sync::Arc;

use crate::error::Result;
use crate::parser::ast::BinaryOperator;
use crate::plan::{BoundExpr, BoundExprKind, RelId};
use crate::storage::{Batch, Bound, Column, Schema, Selection, Table, DEFAULT_BATCH_SIZE};
use crate::types::{value_class, ScalarValue};

use super::{Operator, OperatorStats, Timer};

/// Above this share of the table, a full scan wins: the gathers stop being
/// occasional random reads and start being a worse version of a sequential
/// pass. Measured in `benches/indexes.sql`.
pub const MAX_INDEX_SELECTIVITY: f64 = 0.05;

/// ...but never a cap below one batch.
///
/// A result that fits in a single batch is one gather of at most 2048 rows,
/// which cannot lose to reading whole row groups and filtering them. Without
/// this floor a small table could never use an index at all -- five percent of
/// ten rows rounds to zero -- and the index path would go untested on exactly
/// the fixtures the corpus is built from.
pub const MIN_INDEX_ROWS: usize = DEFAULT_BATCH_SIZE;

/// How many matching rows still make an index scan worthwhile.
pub fn index_row_cap(table_rows: usize) -> usize {
    (((table_rows as f64) * MAX_INDEX_SELECTIVITY) as usize).max(MIN_INDEX_ROWS)
}

/// A closed-or-open interval on the indexed column, derived from a predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexRange {
    pub lower: Bound,
    pub upper: Bound,
}

impl IndexRange {
    fn unbounded() -> IndexRange {
        IndexRange {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
        }
    }

    fn is_unbounded(&self) -> bool {
        self.lower == Bound::Unbounded && self.upper == Bound::Unbounded
    }

    pub fn to_sql(&self, column: &str) -> String {
        match (&self.lower, &self.upper) {
            (Bound::Included(a), Bound::Included(b)) if a == b => {
                format!("{column} = {}", lit_sql(a))
            }
            _ => {
                let lo = match &self.lower {
                    Bound::Unbounded => String::new(),
                    Bound::Included(v) => format!("{} <= ", lit_sql(v)),
                    Bound::Excluded(v) => format!("{} < ", lit_sql(v)),
                };
                let hi = match &self.upper {
                    Bound::Unbounded => String::new(),
                    Bound::Included(v) => format!(" <= {}", lit_sql(v)),
                    Bound::Excluded(v) => format!(" < {}", lit_sql(v)),
                };
                format!("{lo}{column}{hi}")
            }
        }
    }
}

/// Narrow a range with everything a predicate says about the indexed column.
///
/// Returns `None` when the predicate constrains the column not at all, since an
/// unbounded index scan is a full scan that also pays for a gather.
pub fn extract_range(
    predicate: &BoundExpr,
    rel: RelId,
    column: usize,
    column_type_class: u8,
) -> Option<IndexRange> {
    let mut range = IndexRange::unbounded();
    narrow(predicate, rel, column, column_type_class, &mut range);
    if range.is_unbounded() {
        None
    } else {
        Some(range)
    }
}

fn narrow(
    expr: &BoundExpr,
    rel: RelId,
    column: usize,
    class: u8,
    range: &mut IndexRange,
) {
    match &expr.kind {
        // Only AND. Every conjunct must hold, so each one may narrow the range
        // independently. An OR could not: a range covering both arms would have
        // to be their union, and this representation holds one interval.
        BoundExprKind::Binary {
            op: BinaryOperator::And,
            left,
            right,
        } => {
            narrow(left, rel, column, class, range);
            narrow(right, rel, column, class, range);
        }

        BoundExprKind::Binary { op, left, right } if op.is_comparison() => {
            if let (Some(()), Some(v)) = (is_column(left, rel, column), literal(right, class)) {
                apply(*op, v, range);
            } else if let (Some(v), Some(())) = (literal(left, class), is_column(right, rel, column))
            {
                // `5 < x` is `x > 5`.
                apply(flip(*op), v, range);
            }
        }

        BoundExprKind::Between {
            expr: subject,
            low,
            high,
            negated: false,
        } if is_column(subject, rel, column).is_some() => {
            if let Some(v) = literal(low, class) {
                apply(BinaryOperator::GtEq, v, range);
            }
            if let Some(v) = literal(high, class) {
                apply(BinaryOperator::LtEq, v, range);
            }
        }

        // Anything else -- OR, NOT, IS NULL, LIKE, arithmetic, a comparison
        // against another column -- says nothing this interval can express. The
        // filter above still applies it.
        _ => {}
    }
}

fn apply(op: BinaryOperator, v: &ScalarValue, range: &mut IndexRange) {
    match op {
        BinaryOperator::Eq => {
            tighten_lower(range, Bound::Included(v.clone()));
            tighten_upper(range, Bound::Included(v.clone()));
        }
        BinaryOperator::Lt => tighten_upper(range, Bound::Excluded(v.clone())),
        BinaryOperator::LtEq => tighten_upper(range, Bound::Included(v.clone())),
        BinaryOperator::Gt => tighten_lower(range, Bound::Excluded(v.clone())),
        BinaryOperator::GtEq => tighten_lower(range, Bound::Included(v.clone())),
        // `<>` would need the union of two ranges, and `IS DISTINCT FROM`
        // deliberately matches NULLs, which are not in the tree at all.
        _ => {}
    }
}

/// Keep whichever lower bound admits fewer rows.
fn tighten_lower(range: &mut IndexRange, candidate: Bound) {
    let keep = match (&range.lower, &candidate) {
        (Bound::Unbounded, _) => true,
        (_, Bound::Unbounded) => false,
        (a, b) => match crate::types::compare(bound_value(a), bound_value(b)) {
            Some(std::cmp::Ordering::Less) => true,
            Some(std::cmp::Ordering::Equal) => matches!(b, Bound::Excluded(_)),
            _ => false,
        },
    };
    if keep {
        range.lower = candidate;
    }
}

fn tighten_upper(range: &mut IndexRange, candidate: Bound) {
    let keep = match (&range.upper, &candidate) {
        (Bound::Unbounded, _) => true,
        (_, Bound::Unbounded) => false,
        (a, b) => match crate::types::compare(bound_value(a), bound_value(b)) {
            Some(std::cmp::Ordering::Greater) => true,
            Some(std::cmp::Ordering::Equal) => matches!(b, Bound::Excluded(_)),
            _ => false,
        },
    };
    if keep {
        range.upper = candidate;
    }
}

fn lit_sql(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Utf8(t) => format!("'{t}'"),
        other => other.to_string(),
    }
}

fn bound_value(b: &Bound) -> &ScalarValue {
    match b {
        Bound::Included(v) | Bound::Excluded(v) => v,
        Bound::Unbounded => unreachable!("callers handle Unbounded before comparing"),
    }
}

fn is_column(expr: &BoundExpr, rel: RelId, column: usize) -> Option<()> {
    match &expr.kind {
        BoundExprKind::Column { rel: r, index, .. } if *r == rel && *index == column => Some(()),
        _ => None,
    }
}

/// A literal of a type the index can compare exactly against its keys.
///
/// A NULL is refused: `col = NULL` is UNKNOWN for every row, so the answer is
/// no rows, and letting the filter reach that conclusion is simpler than
/// inventing an empty range for it.
fn literal(expr: &BoundExpr, class: u8) -> Option<&ScalarValue> {
    match &expr.kind {
        BoundExprKind::Literal(v) if !v.is_null() && value_class(&v.data_type()) == class => Some(v),
        _ => None,
    }
}

fn flip(op: BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// The operator
// ---------------------------------------------------------------------------

/// Emits exactly the rows an index lookup named, in table order.
///
/// Producing them in table order is not incidental. An index scan is meant to
/// be a faster route to the same answer, and for a query with no `ORDER BY`
/// "the same answer" includes the order a full scan would have produced --
/// otherwise swapping in an index would quietly change results that no clause
/// pinned down.
pub struct IndexScanExec {
    table: Arc<Table>,
    batch_size: usize,
    projection: Option<Vec<usize>>,
    schema: Arc<Schema>,
    /// Global row ids, ascending.
    rows: Vec<u32>,
    cursor: usize,
    /// First global row id of each row group, for mapping global to local.
    group_starts: Vec<u32>,
    stats: OperatorStats,
}

impl IndexScanExec {
    pub fn new(
        table: Arc<Table>,
        rows: Vec<u32>,
        column_name: &str,
        range: &IndexRange,
    ) -> IndexScanExec {
        let mut stats = OperatorStats::new(
            "IndexScan",
            format!(
                "table={}, index on {} ({})",
                table.name,
                column_name,
                range.to_sql(column_name)
            ),
        );
        stats.row_groups_total = table.num_row_groups() as u64;
        // Not an estimate. The lookup already walked the leaves, so this
        // operator knows exactly how many rows it will emit -- which is the
        // same fact the planner used to choose it. Reporting the optimizer's
        // guess here instead would show a q-error of thousands for a decision
        // that was made on the true number.
        stats.estimated_rows = Some(rows.len() as f64);

        let group_starts = table.row_group_starts();
        // How many row groups the gather will actually touch. Reporting it
        // alongside the zone-map counter lets the pipeline viewer compare the
        // two strategies on the same axis.
        let mut touched = 0u64;
        let mut last = usize::MAX;
        for r in &rows {
            let g = group_of(&group_starts, *r);
            if g != last {
                touched += 1;
                last = g;
            }
        }
        stats.row_groups_scanned = touched;
        stats.row_groups_pruned = table.num_row_groups() as u64 - touched;

        let schema = Arc::clone(&table.schema);
        IndexScanExec {
            table,
            batch_size: DEFAULT_BATCH_SIZE,
            projection: None,
            schema,
            rows,
            cursor: 0,
            group_starts,
            stats,
        }
    }

    pub fn with_batch_size(mut self, n: usize) -> IndexScanExec {
        self.batch_size = n.max(1);
        self
    }

    pub fn with_projection(
        mut self,
        projection: Option<Vec<usize>>,
        schema: Arc<Schema>,
    ) -> IndexScanExec {
        if let Some(p) = &projection {
            let pruned = self.table.schema.len() - p.len();
            if pruned > 0 {
                self.stats.detail = format!("{}, {pruned} column(s) pruned", self.stats.detail);
            }
        }
        self.projection = projection;
        self.schema = schema;
        self
    }
}

/// Which row group a global row id falls in.
fn group_of(starts: &[u32], row: u32) -> usize {
    // `starts` is ascending, so the group is the last start at or below `row`.
    starts.partition_point(|s| *s <= row) - 1
}

impl Operator for IndexScanExec {
    /// Deliberately ignored: see the constructor. The count set there is exact.
    fn set_estimated_rows(&mut self, _rows: f64) {}

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        let timer = Timer::start();
        let result = if self.cursor >= self.rows.len() {
            None
        } else {
            // One batch never spans two row groups: a gather that stays inside
            // one group keeps the columns it touches in cache, and the
            // bookkeeping stays a subtraction.
            let group = group_of(&self.group_starts, self.rows[self.cursor]);
            let start = self.group_starts[group];
            let end = start + self.table.row_groups[group].num_rows as u32;

            let mut local = Vec::with_capacity(self.batch_size.min(self.rows.len() - self.cursor));
            while self.cursor < self.rows.len() && local.len() < self.batch_size {
                let row = self.rows[self.cursor];
                if row >= end {
                    break;
                }
                local.push((row - start) as usize);
                self.cursor += 1;
            }

            let rg = &self.table.row_groups[group];
            let sources: Vec<Arc<Column>> = match &self.projection {
                Some(p) => p.iter().map(|i| rg.column(*i)).collect::<Result<_>>()?,
                None => rg.all_columns()?,
            };
            let columns: Vec<Arc<Column>> =
                sources.iter().map(|c| Arc::new(c.take(&local))).collect();
            Some(Batch::new(
                Arc::clone(&self.schema),
                columns,
                Selection::Range {
                    offset: 0,
                    len: local.len(),
                },
            ))
        };

        self.stats.elapsed_nanos += timer.elapsed_nanos();
        if let Some(b) = &result {
            self.stats.rows_in += b.num_rows() as u64;
            self.stats.record_output(b);
        }
        Ok(result)
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }
}
