//! Statistics and cardinality estimation.
//!
//! Everything a cost model decides rests on one question: how many rows will
//! this operator produce? The answer is always a guess, and the guesses compound
//! multiplicatively -- a 2x error per join is 32x by the fifth. This module is
//! where the guesses are made, and the engine reports them next to the truth
//! after execution rather than hiding them.
//!
//! ## What is collected, per column
//!
//! * row count, null count -- exact
//! * distinct count -- HyperLogLog, ~1.6% error in 4 KiB
//! * min / max -- exact, aggregated from the row-group zone maps
//! * an equi-depth histogram over a sample, for ranges
//! * a most-common-values list, for equality on skewed columns
//!
//! The histogram and the MCV list answer different questions and neither
//! subsumes the other. A histogram says how many rows fall in a *range*; it is
//! useless for equality, because an equi-depth bucket boundary tells you nothing
//! about how often one particular value occurs. The MCV list is the opposite:
//! exact for the values it holds, silent about everything else. Together they
//! cover `x > 5` and `x = 'the one value that is 40% of the table'`.
//!
//! ## Where these estimates go wrong
//!
//! Conjunctions are multiplied as if the predicates were independent. They
//! usually are not -- `city = 'London' AND country = 'UK'` is one predicate
//! wearing two hats, and multiplying underestimates by the correlation. This is
//! the single largest source of error in every optimizer that does it this way,
//! and every optimizer does it this way.

pub mod histogram;
pub mod hll;

use std::collections::HashMap;
use std::sync::Arc;

use crate::catalog::Catalog;
use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind, JoinType, LogicalPlan, RelId};
use crate::storage::Table;
use crate::types::ScalarValue;

pub use histogram::Histogram;
pub use hll::HyperLogLog;

/// Values sampled per column when building a histogram. Sorting every value of
/// a million-row table at load time is a second of work for an estimate that
/// does not get meaningfully better past a sample this size.
const HISTOGRAM_SAMPLE: usize = 20_000;

/// How many most-common values to keep per column.
const MCV_COUNT: usize = 24;

/// Cap on the distinct values tracked while looking for common ones. A column
/// with more than this is not skewed in a way an MCV list can capture.
/// Row groups decoded to build distribution statistics for a lazily loaded
/// table. One 64K group is a large enough sample for a histogram that is itself
/// built from 20K rows, and it keeps registration from reading the whole file.
const SAMPLED_ROW_GROUPS: usize = 1;

const MCV_TRACKING_LIMIT: usize = 50_000;

// Fallbacks, used when a column has no usable statistics. They are guesses, and
// they are named so that a bad estimate can be traced to one of them rather
// than to arithmetic.
pub const DEFAULT_SELECTIVITY: f64 = 0.25;
pub const DEFAULT_EQUALITY_SELECTIVITY: f64 = 0.005;
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 0.3;
pub const DEFAULT_LIKE_SELECTIVITY: f64 = 0.1;

#[derive(Debug, Clone, Default)]
pub struct ColumnStatistics {
    pub null_count: usize,
    pub distinct_count: f64,
    pub min: Option<ScalarValue>,
    pub max: Option<ScalarValue>,
    pub histogram: Option<Histogram>,
    /// Value and its row count, most frequent first.
    pub most_common: Vec<(ScalarValue, usize)>,
}

impl ColumnStatistics {
    /// Rows matching `= value`, as a fraction, using the MCV list when it knows
    /// the value and falling back to a uniform assumption otherwise.
    fn equality_selectivity(&self, value: &ScalarValue, rows: f64) -> f64 {
        if rows <= 0.0 {
            return 0.0;
        }
        if value.is_null() {
            // `x = NULL` is UNKNOWN for every row, so nothing passes.
            return 0.0;
        }
        for (v, count) in &self.most_common {
            if v == value {
                return *count as f64 / rows;
            }
        }
        // Not a common value. The remaining rows are spread over the remaining
        // distinct values, which is a better assumption than 1/distinct when
        // the MCVs account for a large share of the table.
        let mcv_rows: usize = self.most_common.iter().map(|(_, c)| *c).sum();
        let remaining_rows = (rows - mcv_rows as f64 - self.null_count as f64).max(0.0);
        let remaining_distinct =
            (self.distinct_count - self.most_common.len() as f64).max(1.0);
        if remaining_rows <= 0.0 {
            return 0.0;
        }
        (remaining_rows / remaining_distinct / rows).clamp(0.0, 1.0)
    }

    fn non_null_fraction(&self, rows: f64) -> f64 {
        if rows <= 0.0 {
            0.0
        } else {
            1.0 - (self.null_count as f64 / rows)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TableStatistics {
    pub row_count: usize,
    pub columns: Vec<ColumnStatistics>,
}

impl TableStatistics {
    /// One pass over the table. Called when a table enters the catalog, so
    /// every query planned afterwards has statistics to work with.
    /// Collect statistics for a table.
    ///
    /// Exact bounds and null counts come from the row groups' own metadata, so
    /// they cost nothing and cover every row. The distribution statistics --
    /// distinct counts, histograms, most-common values -- have to look at
    /// values, and for a lazily loaded table that means decoding.
    ///
    /// So they are built from a *sample of row groups*: all of them when the
    /// data is already in memory, and the first
    /// [`SAMPLED_ROW_GROUPS`] when it is not. Reading a whole Parquet file at
    /// registration to build a histogram would give back exactly the laziness
    /// the format exists to provide, and a histogram over 64K rows is not
    /// meaningfully worse than one over a million -- it was already built from
    /// a 20K-row sample.
    pub fn analyze(table: &Table) -> TableStatistics {
        let row_count = table.num_rows();
        let width = table.schema.len();

        let sampled: Vec<&crate::storage::RowGroup> = if table.is_pending() {
            table.row_groups.iter().take(SAMPLED_ROW_GROUPS).collect()
        } else {
            table.row_groups.iter().collect()
        };
        let sampled_rows: usize = sampled.iter().map(|rg| rg.num_rows).sum();
        let stride = (sampled_rows / HISTOGRAM_SAMPLE).max(1);

        let mut hlls = vec![HyperLogLog::new(); width];
        let mut samples: Vec<Vec<ScalarValue>> = vec![Vec::new(); width];
        // Keyed by value hash so that floats and strings can share one map;
        // a collision would misreport one MCV frequency, which is a rounding
        // error in an estimate rather than a correctness problem.
        let mut counts: Vec<HashMap<u64, (ScalarValue, usize)>> = vec![HashMap::new(); width];

        let mut row_index = 0usize;
        for rg in &sampled {
            for c in 0..width {
                // A column that cannot be decoded leaves this statistic empty
                // rather than failing the load; the estimator degrades, the
                // query still runs, and the scan will report the real error.
                let Ok(column) = rg.column(c) else { continue };
                for row in 0..rg.num_rows {
                    let value = column.value(row);
                    if value.is_null() {
                        continue;
                    }
                    let hash = hll::hash_scalar(&value);
                    hlls[c].add_hash(hash);
                    if (row_index + row).is_multiple_of(stride)
                        && samples[c].len() < HISTOGRAM_SAMPLE
                    {
                        samples[c].push(value.clone());
                    }
                    let map = &mut counts[c];
                    if let Some((_, n)) = map.get_mut(&hash) {
                        *n += 1;
                    } else if map.len() < MCV_TRACKING_LIMIT {
                        map.insert(hash, (value, 1));
                    }
                }
            }
            row_index += rg.num_rows;
        }

        // Null counts are exact from the zone maps, over every row group --
        // never only the sampled ones.
        let mut nulls = vec![0usize; width];
        for rg in &table.row_groups {
            for (c, s) in rg.stats.iter().enumerate().take(width) {
                nulls[c] += s.null_count;
            }
        }

        let scale = if sampled_rows == 0 {
            1.0
        } else {
            row_count as f64 / sampled_rows as f64
        };

        let columns = (0..width)
            .map(|c| {
                let (min, max) = table
                    .row_groups
                    .iter()
                    .fold((None, None), |(lo, hi), rg| {
                        let s = &rg.stats[c];
                        (merge_min(lo, s.min.clone()), merge_max(hi, s.max.clone()))
                    });

                let mut most_common: Vec<(ScalarValue, usize)> =
                    std::mem::take(&mut counts[c]).into_values().collect();
                most_common.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                most_common.truncate(MCV_COUNT);
                // A value that occurs once is not "common"; keeping it would
                // just make the equality fallback worse.
                most_common.retain(|(_, n)| *n > 1);
                for (_, n) in &mut most_common {
                    *n = (*n as f64 * scale).round() as usize;
                }

                ColumnStatistics {
                    null_count: nulls[c],
                    distinct_count: extrapolate_distinct(
                        hlls[c].estimate(),
                        sampled_rows,
                        row_count,
                    ),
                    min,
                    max,
                    histogram: Histogram::build(&mut samples[c], histogram::DEFAULT_BUCKETS),
                    most_common,
                }
            })
            .collect();

        TableStatistics { row_count, columns }
    }
}

/// Project a sample's distinct count onto the whole table.
///
/// Linear scaling is right for a column where every value is distinct and badly
/// wrong for one with three values -- a `vendor` column sampled from one row
/// group in sixteen would come back with forty-eight. So the scaling is
/// weighted by how distinct the sample actually was: a sample where every row
/// held a new value scales fully, one where almost none did barely scales at
/// all, and the cases in between interpolate.
fn extrapolate_distinct(sample_distinct: f64, sampled_rows: usize, total_rows: usize) -> f64 {
    if sampled_rows == 0 || total_rows <= sampled_rows {
        return sample_distinct;
    }
    let ratio = (sample_distinct / sampled_rows as f64).clamp(0.0, 1.0);
    let growth = total_rows as f64 / sampled_rows as f64;
    let scaled = sample_distinct * (1.0 + (growth - 1.0) * ratio);
    scaled.min(total_rows as f64)
}

fn merge_min(a: Option<ScalarValue>, b: Option<ScalarValue>) -> Option<ScalarValue> {
    match (a, b) {
        (Some(a), Some(b)) => Some(
            if crate::types::compare(&a, &b) == Some(std::cmp::Ordering::Less) {
                a
            } else {
                b
            },
        ),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

fn merge_max(a: Option<ScalarValue>, b: Option<ScalarValue>) -> Option<ScalarValue> {
    match (a, b) {
        (Some(a), Some(b)) => Some(
            if crate::types::compare(&a, &b) == Some(std::cmp::Ordering::Greater) {
                a
            } else {
                b
            },
        ),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Cardinality estimation
// ---------------------------------------------------------------------------

/// What an operator is expected to produce.
#[derive(Debug, Clone, Default)]
pub struct Estimate {
    pub rows: f64,
    /// Per-column state, keyed by the relation and column a reference names.
    pub columns: HashMap<(RelId, usize), ColumnEstimate>,
}

#[derive(Debug, Clone)]
pub struct ColumnEstimate {
    pub distinct: f64,
    /// Base statistics, for histogram and MCV lookups. Shared, not scaled --
    /// selectivity is computed against the base and then applied to the current
    /// row count.
    pub stats: Arc<ColumnStatistics>,
    /// Rows the statistics were collected over.
    pub base_rows: f64,
}

/// Estimate every node of a plan, keyed by relation id.
pub fn estimate_all(plan: &LogicalPlan, catalog: &Catalog) -> HashMap<RelId, f64> {
    let mut out = HashMap::new();
    estimate(plan, catalog, &mut out);
    out
}

pub fn estimate_rows(plan: &LogicalPlan, catalog: &Catalog) -> f64 {
    let mut out = HashMap::new();
    estimate(plan, catalog, &mut out).rows
}

/// The full estimate for a plan, including per-column state. Join reordering
/// needs this: it composes candidate joins without ever building them.
pub fn estimate_plan(plan: &LogicalPlan, catalog: &Catalog) -> Estimate {
    let mut out = HashMap::new();
    estimate(plan, catalog, &mut out)
}

/// The estimate for joining two already-estimated inputs on `on`.
pub fn estimate_join(left: &Estimate, right: &Estimate, on: Option<&BoundExpr>) -> Estimate {
    let mut columns = left.columns.clone();
    columns.extend(right.columns.clone());
    let rows = join_cardinality(JoinType::Inner, on, left, right, &columns);
    Estimate {
        columns: scale_distinct(columns, rows),
        rows,
    }
}

fn estimate(
    plan: &LogicalPlan,
    catalog: &Catalog,
    out: &mut HashMap<RelId, f64>,
) -> Estimate {
    let result = match plan {
        LogicalPlan::OneRow { .. } => Estimate {
            rows: 1.0,
            columns: HashMap::new(),
        },

        LogicalPlan::Scan { rel, table_name, table_schema, .. } => {
            let stats = catalog.statistics(table_name);
            let rows = stats.as_ref().map_or(0.0, |s| s.row_count as f64);
            let mut columns = HashMap::new();
            if let Some(stats) = &stats {
                for index in 0..table_schema.len() {
                    if let Some(c) = stats.columns.get(index) {
                        columns.insert(
                            (*rel, index),
                            ColumnEstimate {
                                distinct: c.distinct_count.max(1.0),
                                stats: Arc::new(c.clone()),
                                base_rows: rows,
                            },
                        );
                    }
                }
            }
            Estimate { rows, columns }
        }

        LogicalPlan::Filter { predicate, input, .. } => {
            let input = estimate(input, catalog, out);
            let selectivity = selectivity(predicate, &input);
            let rows = (input.rows * selectivity).max(0.0);
            Estimate {
                // A filter cannot increase the distinct count of a column, and
                // cannot leave more distinct values than surviving rows. That is
                // a crude rule -- the real relationship depends on which rows
                // survive -- but it is monotone and cheap.
                columns: scale_distinct(input.columns, rows),
                rows,
            }
        }

        LogicalPlan::Project { input, .. } => estimate(input, catalog, out),

        // Neither sorting nor windowing changes how many rows there are.
        LogicalPlan::Sort { input, .. } => estimate(input, catalog, out),
        LogicalPlan::Window { input, .. } => estimate(input, catalog, out),

        LogicalPlan::Distinct { input, .. } => {
            let inner = estimate(input, catalog, out);
            // Assume the row as a whole is as discriminating as its most
            // discriminating column -- a guess, but a monotone one that never
            // exceeds the input.
            let distinct = inner
                .columns
                .values()
                .map(|c| c.distinct)
                .fold(1.0f64, f64::max);
            Estimate {
                rows: distinct.clamp(1.0, inner.rows.max(1.0)),
                columns: inner.columns,
            }
        }

        LogicalPlan::SetOp { op, all, left, right, .. } => {
            let l = estimate(left, catalog, out);
            let r = estimate(right, catalog, out);
            let rows = match (op, all) {
                (crate::plan::SetOperator::Union, true) => l.rows + r.rows,
                // With duplicates removed the answer is somewhere between the
                // larger input and their sum; halfway is as good a guess as
                // any without knowing the overlap.
                (crate::plan::SetOperator::Union, false) => {
                    l.rows.max(r.rows) + (l.rows + r.rows) * 0.25
                }
                (crate::plan::SetOperator::Intersect, _) => l.rows.min(r.rows) * 0.5,
                (crate::plan::SetOperator::Except, _) => (l.rows - r.rows * 0.5).max(0.0),
            };
            Estimate {
                rows,
                columns: HashMap::new(),
            }
        }

        // Naming a subquery's output does not change how many rows it has, but
        // it does rename the relation those columns belong to.
        LogicalPlan::SubqueryAlias { rel, schema, input, .. } => {
            let inner = estimate(input, catalog, out);
            let mut columns = HashMap::new();
            for index in 0..schema.len() {
                if let Some(base) = inner.columns.values().nth(index) {
                    columns.insert((*rel, index), base.clone());
                }
            }
            Estimate {
                rows: inner.rows,
                columns,
            }
        }

        LogicalPlan::Limit { skip, fetch, input, .. } => {
            let input = estimate(input, catalog, out);
            let after_skip = (input.rows - *skip as f64).max(0.0);
            let rows = match fetch {
                Some(n) => after_skip.min(*n as f64),
                None => after_skip,
            };
            Estimate {
                columns: scale_distinct(input.columns, rows),
                rows,
            }
        }

        LogicalPlan::Join { join_type, on, left, right, .. } => {
            let l = estimate(left, catalog, out);
            let r = estimate(right, catalog, out);
            let mut columns = l.columns.clone();
            columns.extend(r.columns.clone());

            let inner = join_cardinality(*join_type, on.as_ref(), &l, &r, &columns);
            // An outer join emits at least every row of the preserved side.
            let rows = match join_type {
                JoinType::Left => inner.max(l.rows),
                JoinType::Right => inner.max(r.rows),
                JoinType::Full => inner.max(l.rows + r.rows),
                _ => inner,
            };
            Estimate {
                columns: scale_distinct(columns, rows),
                rows,
            }
        }

        LogicalPlan::Aggregate { rel, group_exprs, input, .. } => {
            let input = estimate(input, catalog, out);
            let rows = if group_exprs.is_empty() {
                // A global aggregate always produces exactly one row, even over
                // an empty input.
                1.0
            } else {
                // Assume grouping keys are independent, so the number of groups
                // is the product of their distinct counts -- capped at the
                // input, since there cannot be more groups than rows.
                group_exprs
                    .iter()
                    .map(|e| distinct_of(e, &input))
                    .product::<f64>()
                    .clamp(1.0, input.rows.max(1.0))
            };
            let mut columns = HashMap::new();
            for (index, e) in group_exprs.iter().enumerate() {
                if let Some(base) = column_estimate(e, &input) {
                    columns.insert(
                        (*rel, index),
                        ColumnEstimate {
                            distinct: base.distinct.min(rows),
                            ..base.clone()
                        },
                    );
                }
            }
            Estimate { rows, columns }
        }
    };

    out.insert(plan.rel(), result.rows);
    result
}

fn scale_distinct(
    mut columns: HashMap<(RelId, usize), ColumnEstimate>,
    rows: f64,
) -> HashMap<(RelId, usize), ColumnEstimate> {
    for c in columns.values_mut() {
        c.distinct = c.distinct.min(rows.max(1.0));
    }
    columns
}

/// The textbook equi-join estimate: `|R| * |S| / max(distinct_R, distinct_S)`.
///
/// The intuition is that each row of the smaller-cardinality side matches
/// `|other| / distinct_other` rows. Taking the *maximum* of the two distinct
/// counts is the containment assumption -- the side with fewer distinct values
/// is assumed to have all of them present in the other. It is optimistic when
/// that is false, which is why join estimates degrade as they compose.
fn join_cardinality(
    join_type: JoinType,
    on: Option<&BoundExpr>,
    l: &Estimate,
    r: &Estimate,
    columns: &HashMap<(RelId, usize), ColumnEstimate>,
) -> f64 {
    let product = l.rows * r.rows;
    if join_type == JoinType::Cross || on.is_none() {
        return product;
    }
    let on = on.expect("checked");

    let mut conjuncts = Vec::new();
    on.clone().split_conjuncts(&mut conjuncts);

    let mut rows = product;
    let mut used_key = false;
    for c in &conjuncts {
        let BoundExprKind::Binary { op: BinaryOperator::Eq, left, right } = &c.kind else {
            continue;
        };
        let (Some(a), Some(b)) = (
            column_ref(left).and_then(|k| columns.get(&k)),
            column_ref(right).and_then(|k| columns.get(&k)),
        ) else {
            continue;
        };
        rows /= a.distinct.max(b.distinct).max(1.0);
        used_key = true;
    }

    if !used_key {
        // No equality to reason about: fall back to a generic selectivity on
        // the cross product.
        rows = product * DEFAULT_SELECTIVITY;
    }

    // Conditions that are not equi-keys still filter.
    for c in &conjuncts {
        if matches!(&c.kind, BoundExprKind::Binary { op: BinaryOperator::Eq, .. }) {
            continue;
        }
        rows *= selectivity_with(c, columns);
    }
    rows.max(0.0)
}

fn column_ref(e: &BoundExpr) -> Option<(RelId, usize)> {
    match &e.kind {
        BoundExprKind::Column { rel, index, .. } => Some((*rel, *index)),
        // See through an implicit widening cast, which the binder inserts on
        // join keys of different integer widths.
        BoundExprKind::Cast { expr, implicit: true } => column_ref(expr),
        _ => None,
    }
}

fn column_estimate<'a>(e: &BoundExpr, est: &'a Estimate) -> Option<&'a ColumnEstimate> {
    column_ref(e).and_then(|k| est.columns.get(&k))
}

fn distinct_of(e: &BoundExpr, est: &Estimate) -> f64 {
    match column_estimate(e, est) {
        Some(c) => c.distinct.max(1.0),
        // An expression over columns has an unknown distinct count; assume it
        // is as discriminating as a tenth of the input.
        None => (est.rows * 0.1).max(1.0),
    }
}

// ---------------------------------------------------------------------------
// Selectivity
// ---------------------------------------------------------------------------

pub fn selectivity(predicate: &BoundExpr, input: &Estimate) -> f64 {
    selectivity_with(predicate, &input.columns)
}

/// Selectivity is computed against the *base* row count each column's
/// statistics were gathered over, not against the current estimate -- which is
/// why the current row count is not a parameter here. A fraction derived from
/// the statistics is then applied to whatever the input has shrunk to.
fn selectivity_with(
    predicate: &BoundExpr,
    columns: &HashMap<(RelId, usize), ColumnEstimate>,
) -> f64 {
    let s = match &predicate.kind {
        BoundExprKind::Literal(ScalarValue::Boolean(true)) => 1.0,
        BoundExprKind::Literal(ScalarValue::Boolean(false))
        | BoundExprKind::Literal(ScalarValue::Null) => 0.0,

        BoundExprKind::Binary { op: BinaryOperator::And, left, right } => {
            // Independence. This is where estimates go wrong: correlated
            // predicates get multiplied as if they were unrelated, and the
            // result is too small by however correlated they are.
            selectivity_with(left, columns) * selectivity_with(right, columns)
        }
        BoundExprKind::Binary { op: BinaryOperator::Or, left, right } => {
            let a = selectivity_with(left, columns);
            let b = selectivity_with(right, columns);
            // Inclusion-exclusion, again assuming independence.
            (a + b - a * b).clamp(0.0, 1.0)
        }
        BoundExprKind::Unary { op: UnaryOperator::Not, expr } => {
            1.0 - selectivity_with(expr, columns)
        }

        BoundExprKind::IsNull { expr, negated } => match column_lookup(expr, columns) {
            Some(c) => {
                let null_fraction = if c.base_rows > 0.0 {
                    c.stats.null_count as f64 / c.base_rows
                } else {
                    0.0
                };
                if *negated {
                    1.0 - null_fraction
                } else {
                    null_fraction
                }
            }
            None => DEFAULT_SELECTIVITY,
        },

        BoundExprKind::Binary { op, left, right } if op.is_comparison() => {
            comparison_selectivity(*op, left, right, columns)
        }

        BoundExprKind::Between { expr, low, high, negated } => {
            let inside = match (column_lookup(expr, columns), literal(low), literal(high)) {
                (Some(c), Some(lo), Some(hi)) => match &c.stats.histogram {
                    Some(h) => h.fraction_between(lo, hi) * c.stats.non_null_fraction(c.base_rows),
                    None => DEFAULT_RANGE_SELECTIVITY,
                },
                _ => DEFAULT_RANGE_SELECTIVITY,
            };
            if *negated {
                1.0 - inside
            } else {
                inside
            }
        }

        BoundExprKind::InList { expr, list, negated } => {
            let matched = match column_lookup(expr, columns) {
                Some(c) => list
                    .iter()
                    .filter_map(literal)
                    .map(|v| c.stats.equality_selectivity(v, c.base_rows))
                    .sum::<f64>()
                    .clamp(0.0, 1.0),
                None => (DEFAULT_EQUALITY_SELECTIVITY * list.len() as f64).clamp(0.0, 1.0),
            };
            if *negated {
                1.0 - matched
            } else {
                matched
            }
        }

        BoundExprKind::Like { negated, .. } => {
            if *negated {
                1.0 - DEFAULT_LIKE_SELECTIVITY
            } else {
                DEFAULT_LIKE_SELECTIVITY
            }
        }

        // A bare boolean column, a CASE, an expression -- no basis to guess
        // better than the default.
        _ => DEFAULT_SELECTIVITY,
    };
    s.clamp(0.0, 1.0)
}

fn comparison_selectivity(
    op: BinaryOperator,
    left: &BoundExpr,
    right: &BoundExpr,
    columns: &HashMap<(RelId, usize), ColumnEstimate>,
) -> f64 {
    // Normalize to `column OP literal`.
    let (column, value, op) = match (column_lookup(left, columns), literal(right)) {
        (Some(c), Some(v)) => (c, v, op),
        _ => match (literal(left), column_lookup(right, columns)) {
            (Some(v), Some(c)) => (c, v, flip(op)),
            _ => {
                // Two columns compared to each other: a join-style predicate
                // that is not being used as a key here.
                return DEFAULT_SELECTIVITY;
            }
        },
    };

    let non_null = column.stats.non_null_fraction(column.base_rows);
    use BinaryOperator::*;
    match op {
        Eq => column.stats.equality_selectivity(value, column.base_rows),
        NotEq => {
            let eq = column.stats.equality_selectivity(value, column.base_rows);
            // `x <> v` is UNKNOWN where x is NULL, so those rows do not pass.
            (non_null - eq).clamp(0.0, 1.0)
        }
        Lt | LtEq | Gt | GtEq => {
            let Some(h) = &column.stats.histogram else {
                return DEFAULT_RANGE_SELECTIVITY;
            };
            let at_most = h.fraction_at_most(value);
            let fraction = match op {
                Lt | LtEq => at_most,
                _ => 1.0 - at_most,
            };
            (fraction * non_null).clamp(0.0, 1.0)
        }
        _ => DEFAULT_SELECTIVITY,
    }
}

fn flip(op: BinaryOperator) -> BinaryOperator {
    use BinaryOperator::*;
    match op {
        Lt => Gt,
        LtEq => GtEq,
        Gt => Lt,
        GtEq => LtEq,
        other => other,
    }
}

fn column_lookup<'a>(
    e: &BoundExpr,
    columns: &'a HashMap<(RelId, usize), ColumnEstimate>,
) -> Option<&'a ColumnEstimate> {
    column_ref(e).and_then(|k| columns.get(&k))
}

fn literal(e: &BoundExpr) -> Option<&ScalarValue> {
    match &e.kind {
        BoundExprKind::Literal(v) => Some(v),
        _ => None,
    }
}

/// Ratio between an estimate and the truth, always >= 1.
///
/// This is the standard way to score an estimator, and it is symmetric on
/// purpose: being 10x over is as wrong as being 10x under, which a plain ratio
/// would not say.
pub fn q_error(estimated: f64, actual: f64) -> f64 {
    let e = estimated.max(1.0);
    let a = actual.max(1.0);
    (e / a).max(a / e)
}
