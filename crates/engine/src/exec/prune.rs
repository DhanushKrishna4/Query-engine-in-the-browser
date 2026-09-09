//! Zone-map pruning: deciding that a row group cannot contain a matching row
//! without reading any of its values.
//!
//! Every row group already carries per-column min/max/null-count, computed once
//! when the data was written. That metadata *is* the index -- the cheapest one
//! there is, because it costs a single pass over data that was being written
//! anyway and it can eliminate 64K rows with two comparisons.
//!
//! The evaluation is three-valued over intervals rather than over values:
//! `ALWAYS FALSE` means no row in the group can satisfy the predicate, so the
//! group is skipped; `ALWAYS TRUE` is tracked only because `NOT` needs it; and
//! `UNKNOWN` means read the group. Being wrong in the `UNKNOWN` direction costs
//! time; being wrong in the `ALWAYS FALSE` direction costs correctness, so every
//! rule here is one-sided and everything unrecognised falls through to
//! `UNKNOWN`.
//!
//! Bloom filters live at the bottom of this file and answer the question zone
//! maps cannot: not "is this value inside the group's range" but "is this exact
//! value in the group at all". They are consulted only after the zone map has
//! failed to rule the group out, since they cost a few hashes and the zone map
//! costs two comparisons.
//!
//! NULLs need care in one direction only. A NULL never satisfies a predicate,
//! so it can never invalidate an `ALWAYS FALSE` verdict -- but it does
//! invalidate `ALWAYS TRUE`, which is why every `AlwaysTrue` below also demands
//! that the column has no NULLs.

use std::cmp::Ordering;

use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind, RelId};
use crate::storage::{BloomFilter, ColumnStats, Schema};
use crate::types::{compare, value_class, ScalarValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    /// No row in this group can satisfy the predicate.
    AlwaysFalse,
    /// Every row satisfies it, NULLs included (so: there are none).
    AlwaysTrue,
    Unknown,
}

impl Certainty {
    fn not(self) -> Certainty {
        match self {
            Certainty::AlwaysFalse => Certainty::AlwaysTrue,
            Certainty::AlwaysTrue => Certainty::AlwaysFalse,
            Certainty::Unknown => Certainty::Unknown,
        }
    }
}

/// Whether a row group could contain a row satisfying `predicate`.
pub fn row_group_can_match(
    predicate: &BoundExpr,
    rel: RelId,
    stats: &[ColumnStats],
    num_rows: usize,
) -> bool {
    evaluate(predicate, rel, stats, num_rows) != Certainty::AlwaysFalse
}

pub fn evaluate(
    expr: &BoundExpr,
    rel: RelId,
    stats: &[ColumnStats],
    num_rows: usize,
) -> Certainty {
    match &expr.kind {
        BoundExprKind::Literal(ScalarValue::Boolean(true)) => Certainty::AlwaysTrue,
        // A predicate that is constantly false or NULL passes no rows.
        BoundExprKind::Literal(ScalarValue::Boolean(false)) | BoundExprKind::Literal(ScalarValue::Null) => {
            Certainty::AlwaysFalse
        }

        BoundExprKind::Unary { op: UnaryOperator::Not, expr } => {
            evaluate(expr, rel, stats, num_rows).not()
        }

        BoundExprKind::Binary { op: BinaryOperator::And, left, right } => {
            let l = evaluate(left, rel, stats, num_rows);
            let r = evaluate(right, rel, stats, num_rows);
            if l == Certainty::AlwaysFalse || r == Certainty::AlwaysFalse {
                Certainty::AlwaysFalse
            } else if l == Certainty::AlwaysTrue && r == Certainty::AlwaysTrue {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }

        BoundExprKind::Binary { op: BinaryOperator::Or, left, right } => {
            let l = evaluate(left, rel, stats, num_rows);
            let r = evaluate(right, rel, stats, num_rows);
            if l == Certainty::AlwaysTrue || r == Certainty::AlwaysTrue {
                Certainty::AlwaysTrue
            } else if l == Certainty::AlwaysFalse && r == Certainty::AlwaysFalse {
                Certainty::AlwaysFalse
            } else {
                Certainty::Unknown
            }
        }

        BoundExprKind::IsNull { expr, negated } => {
            let Some(s) = column_stats(expr, rel, stats) else {
                return Certainty::Unknown;
            };
            let has_null = s.null_count > 0;
            let all_null = s.null_count == num_rows;
            match (negated, has_null, all_null) {
                // IS NULL over a group with no NULLs.
                (false, false, _) => Certainty::AlwaysFalse,
                (false, _, true) => Certainty::AlwaysTrue,
                (true, false, _) => Certainty::AlwaysTrue,
                (true, _, true) => Certainty::AlwaysFalse,
                _ => Certainty::Unknown,
            }
        }

        BoundExprKind::Binary { op, left, right } if op.is_comparison() => {
            match (column_stats(left, rel, stats), literal(right)) {
                (Some(s), Some(v)) => compare_bounds(*op, s, v, num_rows),
                _ => match (literal(left), column_stats(right, rel, stats)) {
                    // `5 < x` is `x > 5` with the operator mirrored.
                    (Some(v), Some(s)) => compare_bounds(flip(*op), s, v, num_rows),
                    _ => Certainty::Unknown,
                },
            }
        }

        BoundExprKind::Between { expr, low, high, negated } => {
            let (Some(s), Some(lo), Some(hi)) = (
                column_stats(expr, rel, stats),
                literal(low),
                literal(high),
            ) else {
                return Certainty::Unknown;
            };
            let inside = {
                let ge = compare_bounds(BinaryOperator::GtEq, s, lo, num_rows);
                let le = compare_bounds(BinaryOperator::LtEq, s, hi, num_rows);
                if ge == Certainty::AlwaysFalse || le == Certainty::AlwaysFalse {
                    Certainty::AlwaysFalse
                } else if ge == Certainty::AlwaysTrue && le == Certainty::AlwaysTrue {
                    Certainty::AlwaysTrue
                } else {
                    Certainty::Unknown
                }
            };
            if *negated {
                inside.not()
            } else {
                inside
            }
        }

        BoundExprKind::InList { expr, list, negated } if !negated => {
            let Some(s) = column_stats(expr, rel, stats) else {
                return Certainty::Unknown;
            };
            let (Some(min), Some(max)) = (&s.min, &s.max) else {
                // Every value is NULL, so nothing can match.
                return Certainty::AlwaysFalse;
            };
            // If no candidate value falls inside the group's range, no row can
            // match -- and a NULL in the list does not change that, since an
            // unmatched row is UNKNOWN rather than TRUE either way.
            let all_outside = list.iter().all(|item| match literal(item) {
                Some(v) if v.is_null() => true,
                Some(v) => {
                    compare(v, min) == Some(Ordering::Less)
                        || compare(v, max) == Some(Ordering::Greater)
                }
                None => false,
            });
            if all_outside && list.iter().all(|i| literal(i).is_some()) {
                Certainty::AlwaysFalse
            } else {
                Certainty::Unknown
            }
        }

        // Anything else -- casts, LIKE, CASE, arithmetic, columns of other
        // relations -- is not something zone maps can rule out.
        _ => Certainty::Unknown,
    }
}

/// Statistics for a bare column reference into the relation being scanned.
fn column_stats<'a>(
    expr: &BoundExpr,
    rel: RelId,
    stats: &'a [ColumnStats],
) -> Option<&'a ColumnStats> {
    match &expr.kind {
        BoundExprKind::Column { rel: r, index, .. } if *r == rel => stats.get(*index),
        _ => None,
    }
}

fn literal(expr: &BoundExpr) -> Option<&ScalarValue> {
    match &expr.kind {
        BoundExprKind::Literal(v) => Some(v),
        _ => None,
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

fn compare_bounds(
    op: BinaryOperator,
    s: &ColumnStats,
    v: &ScalarValue,
    num_rows: usize,
) -> Certainty {
    if v.is_null() {
        // Any comparison against NULL is UNKNOWN for every row, so none pass.
        return Certainty::AlwaysFalse;
    }
    let (Some(min), Some(max)) = (&s.min, &s.max) else {
        // No non-NULL values at all.
        return Certainty::AlwaysFalse;
    };
    let (Some(vs_min), Some(vs_max)) = (compare(v, min), compare(v, max)) else {
        // Not comparable (mismatched types, or NaN bounds).
        return Certainty::Unknown;
    };
    let no_nulls = s.null_count == 0;
    let all_same = compare(min, max) == Some(Ordering::Equal);

    use BinaryOperator::*;
    use Ordering::*;
    match op {
        // x > v
        Gt => {
            if vs_max != Less {
                Certainty::AlwaysFalse
            } else if vs_min == Less && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        // x >= v
        GtEq => {
            if vs_max == Greater {
                Certainty::AlwaysFalse
            } else if vs_min != Greater && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        // x < v
        Lt => {
            if vs_min != Greater {
                Certainty::AlwaysFalse
            } else if vs_max == Greater && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        // x <= v
        LtEq => {
            if vs_min == Less {
                Certainty::AlwaysFalse
            } else if vs_max != Less && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        Eq => {
            if vs_min == Less || vs_max == Greater {
                Certainty::AlwaysFalse
            } else if all_same && vs_min == Equal && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        NotEq => {
            if all_same && vs_min == Equal {
                Certainty::AlwaysFalse
            } else if (vs_min == Less || vs_max == Greater) && no_nulls {
                Certainty::AlwaysTrue
            } else {
                Certainty::Unknown
            }
        }
        _ => {
            let _ = num_rows;
            Certainty::Unknown
        }
    }
}

// ---------------------------------------------------------------------------
// Bloom filters
// ---------------------------------------------------------------------------

/// Whether a row group's bloom filters prove no row can match.
///
/// One-sided in the same direction as everything else here: `true` means
/// certainly no match, `false` means read the group. Only equality against a
/// literal is answerable -- a filter records membership, not order, so it has
/// nothing to say about `<` or `LIKE`.
pub fn bloom_rejects(
    predicate: &BoundExpr,
    rel: RelId,
    blooms: &[Option<BloomFilter>],
    schema: &Schema,
) -> bool {
    match &predicate.kind {
        // Each conjunct must hold, so any one of them proving absence is
        // enough. An OR is not decomposable this way: one arm being absent
        // says nothing about the other.
        BoundExprKind::Binary { op: BinaryOperator::And, left, right } => {
            bloom_rejects(left, rel, blooms, schema) || bloom_rejects(right, rel, blooms, schema)
        }

        BoundExprKind::Binary { op: BinaryOperator::Eq, left, right } => {
            match (bloom_for(left, rel, blooms, schema), literal(right)) {
                (Some(f), Some(v)) => absent(f, v),
                _ => match (literal(left), bloom_for(right, rel, blooms, schema)) {
                    (Some(v), Some(f)) => absent(f, v),
                    _ => false,
                },
            }
        }

        // `x IN (a, b, c)` is a disjunction of equalities, so the group can be
        // skipped only when *every* candidate is absent. A non-literal item
        // makes the whole test inconclusive.
        BoundExprKind::InList { expr, list, negated: false } => {
            let Some(f) = bloom_for(expr, rel, blooms, schema) else {
                return false;
            };
            !list.is_empty()
                && list.iter().all(|item| match literal(item) {
                    // A NULL in the list can never make the predicate TRUE, so
                    // it does not stand in the way of skipping.
                    Some(v) if v.is_null() => true,
                    Some(v) => absent(f, v),
                    None => false,
                })
        }

        _ => false,
    }
}

/// A column's filter, paired with the column's declared type.
#[derive(Clone, Copy)]
struct ColumnFilter<'a> {
    filter: &'a BloomFilter,
    data_type: &'a crate::types::DataType,
}

fn absent(f: ColumnFilter<'_>, v: &ScalarValue) -> bool {
    // A literal from a different value class hashes differently from an equal
    // column value, so the filter would be answering a question we did not ask.
    // Refusing it costs a scan; trusting it would lose rows.
    value_class(&v.data_type()) == value_class(f.data_type) && !f.filter.might_contain(v)
}

/// The filter for a bare column reference into the scanned relation.
fn bloom_for<'a>(
    expr: &BoundExpr,
    rel: RelId,
    blooms: &'a [Option<BloomFilter>],
    schema: &'a Schema,
) -> Option<ColumnFilter<'a>> {
    let BoundExprKind::Column { rel: r, index, .. } = &expr.kind else {
        return None;
    };
    if *r != rel {
        return None;
    }
    let filter = blooms.get(*index)?.as_ref()?;
    Some(ColumnFilter {
        filter,
        data_type: &schema.fields.get(*index)?.data_type,
    })
}
