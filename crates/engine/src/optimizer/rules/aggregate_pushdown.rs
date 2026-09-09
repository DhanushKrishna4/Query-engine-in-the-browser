//! Aggregate pushdown through a join -- eager aggregation.
//!
//! ```text
//!   Aggregate [k, SUM(f.amount)]              Aggregate [k, SUM(partial)]
//!     -> Join f.k = d.k               ->        -> Join p.k = d.k
//!       -> Scan facts   f                         -> Aggregate [k, SUM(amount)]   p
//!       -> Scan dims    d                         |    -> Scan facts
//!                                                 -> Scan dims   d
//! ```
//!
//! The join multiplies rows: each fact row is repeated once per matching
//! dimension row, and the aggregate above then adds up the copies. Aggregating
//! the fact side *first* collapses it to one row per group, and the aggregate
//! above combines the partials. When the fact table has far more rows than
//! groups, the join is handed a fraction of the input.
//!
//! ## Why this needs no uniqueness proof
//!
//! The textbook version of this rewrite pushes the whole aggregate below the
//! join and requires the other side's join key to be unique, so the join cannot
//! duplicate anything. That needs declared keys, which this engine does not
//! have.
//!
//! Eager aggregation needs none, because the aggregate above is still there to
//! fix up the duplication. Take one join key value, with `m` rows on the pushed
//! side and `n` on the other:
//!
//!   * originally `SUM(x)` sees `m * n` rows and returns `n * sum(x)`;
//!   * pushed, the partial returns `sum(x)`, the join makes `n` copies of it,
//!     and the `SUM` above returns `n * sum(x)`. The same.
//!
//! `COUNT(*)` works the same way once it becomes `SUM` above: the partial
//! returns `m`, and summing `n` copies gives `m * n`. `MIN` and `MAX` are
//! idempotent, so `n` copies of the partial minimum still minimise to it.
//!
//! ## What is refused
//!
//! **`AVG` and any `DISTINCT` aggregate.** Neither is decomposable: an average
//! of averages is not the average, and a distinct count of partial distinct
//! counts double-counts across partitions. Splitting `AVG` into `SUM/COUNT`
//! would work and is a different rewrite.
//!
//! **Outer joins.** Padding introduces NULL rows that the partial never saw, so
//! the arithmetic above stops matching. Inner joins only.
//!
//! **Anything but a conjunction of column equalities in the `ON` clause.** The
//! join condition has to be rewritten to read the partial's output, and that is
//! only mechanical when its operands are plain columns.
//!
//! **A rewrite the statistics say will not pay.** The whole point is that the
//! partial is much smaller than its input; if the estimator says it is not, the
//! extra aggregate is pure cost.

use std::sync::Arc;

use crate::optimizer::stats;
use crate::optimizer::{OptimizerContext, Rule};
use crate::parser::ast::BinaryOperator;
use crate::plan::{
    AggregateFunction, BoundAggregate, BoundExpr, BoundExprKind, JoinType, LogicalPlan, RelId,
};
use crate::storage::{Field, Schema};

pub struct AggregatePushdown;

/// The partial must collapse the pushed side by at least this much, or the
/// extra aggregate costs more than the join it saves.
///
/// Deliberately blunt. The estimator's own error is larger than any precision
/// a finer threshold would buy, and being wrong here is a slower plan rather
/// than a wrong one.
const MIN_REDUCTION: f64 = 2.0;

impl Rule for AggregatePushdown {
    fn name(&self) -> &'static str {
        "aggregate_pushdown"
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Aggregate { rel, group_exprs, aggregates, schema, input } = plan else {
            return None;
        };
        let LogicalPlan::Join { rel: join_rel, join_type: JoinType::Inner, on: Some(on), left, right, .. } =
            &**input
        else {
            return None;
        };

        let equi = equi_columns(on)?;
        // Try the left side, then the right. The first that qualifies wins;
        // pushing into both would need two partials and a second proof.
        for push_left in [true, false] {
            let (side, other) = if push_left {
                (left.as_ref(), right.as_ref())
            } else {
                (right.as_ref(), left.as_ref())
            };
            if let Some(rewritten) = push_into(
                *rel,
                group_exprs,
                aggregates,
                schema,
                *join_rel,
                on,
                left,
                right,
                side,
                other,
                push_left,
                &equi,
                ctx,
            ) {
                return Some(rewritten);
            }
        }
        None
    }
}

/// The join keys, as `(left column, right column)` pairs.
///
/// `None` unless the whole condition is a conjunction of equalities between
/// bare columns -- anything else cannot be mechanically rewritten to read the
/// partial's output.
fn equi_columns(on: &BoundExpr) -> Option<Vec<(BoundExpr, BoundExpr)>> {
    let mut conjuncts = Vec::new();
    on.clone().split_conjuncts(&mut conjuncts);
    let mut out = Vec::with_capacity(conjuncts.len());
    for c in conjuncts {
        let BoundExprKind::Binary { op: BinaryOperator::Eq, left, right } = &c.kind else {
            return None;
        };
        if !matches!(left.kind, BoundExprKind::Column { .. })
            || !matches!(right.kind, BoundExprKind::Column { .. })
        {
            return None;
        }
        out.push(((**left).clone(), (**right).clone()));
    }
    (!out.is_empty()).then_some(out)
}

#[allow(clippy::too_many_arguments)]
fn push_into(
    agg_rel: RelId,
    group_exprs: &[BoundExpr],
    aggregates: &[BoundAggregate],
    schema: &Arc<Schema>,
    join_rel: RelId,
    on: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
    side: &LogicalPlan,
    other: &LogicalPlan,
    push_left: bool,
    equi: &[(BoundExpr, BoundExpr)],
    ctx: &mut OptimizerContext,
) -> Option<LogicalPlan> {
    let side_rels = side.relations();
    let other_rels = other.relations();
    let reads_side = |e: &BoundExpr| {
        let mut r = std::collections::BTreeSet::new();
        e.relations(&mut r);
        !r.is_empty() && r.is_subset(&side_rels)
    };
    let reads_other = |e: &BoundExpr| {
        let mut r = std::collections::BTreeSet::new();
        e.relations(&mut r);
        r.is_subset(&other_rels)
    };

    // Every aggregate must be decomposable and read only the pushed side.
    // `COUNT(*)` has no argument and counts rows, which are on both sides --
    // but the arithmetic above still works, so it is allowed.
    for a in aggregates {
        if a.distinct || matches!(a.func, AggregateFunction::Avg) {
            return None;
        }
        if let Some(arg) = &a.arg {
            if !reads_side(arg) {
                return None;
            }
        }
    }
    if aggregates.is_empty() {
        // A `DISTINCT`-style aggregate with no aggregate functions gains
        // nothing here and the combine step below would have nothing to do.
        return None;
    }

    // Group keys split cleanly, or not at all.
    let mut side_groups: Vec<BoundExpr> = Vec::new();
    let mut placement: Vec<Placement> = Vec::with_capacity(group_exprs.len());
    for g in group_exprs {
        if reads_side(g) {
            placement.push(Placement::Pushed(side_groups.len()));
            side_groups.push(g.clone());
        } else if reads_other(g) {
            placement.push(Placement::Kept);
        } else {
            // Straddles both sides; the partial cannot compute it.
            return None;
        }
    }

    // The join keys on this side must survive into the partial's output, or
    // the join above has nothing to match on.
    let key_start = side_groups.len();
    let mut key_slots: Vec<usize> = Vec::with_capacity(equi.len());
    for (l, r) in equi {
        let key = if push_left { l } else { r };
        if !reads_side(key) {
            // The condition does not orient the way this side expects.
            return None;
        }
        match side_groups.iter().position(|g| same_column(g, key)) {
            Some(i) => key_slots.push(i),
            None => {
                key_slots.push(side_groups.len());
                side_groups.push(key.clone());
            }
        }
    }
    let _ = key_start;

    // Is it worth it? The partial is only a win if it collapses its input.
    let input_rows = stats::estimate_rows(side, ctx.catalog);
    let partial = LogicalPlan::Aggregate {
        rel: ctx.ids.allocate(),
        group_exprs: side_groups.clone(),
        aggregates: aggregates
            .iter()
            .map(|a| BoundAggregate {
                func: a.func,
                arg: a.arg.clone(),
                distinct: false,
                data_type: a.data_type,
                nullable: a.nullable,
                span: a.span,
            })
            .collect(),
        schema: Arc::new(Schema::new(
            side_groups
                .iter()
                .enumerate()
                .map(|(i, g)| Field::new(format!("g{i}"), g.data_type, g.nullable))
                .collect::<Vec<_>>()
                .into_iter()
                .chain(aggregates.iter().enumerate().map(|(i, a)| {
                    Field::new(format!("p{i}"), a.data_type, a.nullable)
                }))
                .collect(),
        )),
        input: Box::new(side.clone()),
    };
    let partial_rows = stats::estimate_rows(&partial, ctx.catalog);
    if partial_rows * MIN_REDUCTION > input_rows {
        return None;
    }
    let partial_rel = partial.rel();

    // Rewrite the join condition to read the partial's key columns.
    let mut mapping: Vec<(BoundExpr, usize)> = Vec::new();
    for ((l, r), slot) in equi.iter().zip(&key_slots) {
        mapping.push((if push_left { l.clone() } else { r.clone() }, *slot));
    }
    let new_on = rewrite_columns(on, partial_rel, &mapping)?;

    let (new_left, new_right) = if push_left {
        (partial, right.clone())
    } else {
        (left.clone(), partial)
    };
    let join = LogicalPlan::Join {
        rel: join_rel,
        join_type: JoinType::Inner,
        on: Some(new_on),
        schema: crate::plan::join_schema(JoinType::Inner, &new_left, &new_right),
        left: Box::new(new_left),
        right: Box::new(new_right),
    };

    // The aggregate above combines the partials.
    let new_groups: Vec<BoundExpr> = group_exprs
        .iter()
        .zip(&placement)
        .map(|(g, p)| match p {
            Placement::Pushed(slot) => column_of(g, partial_rel, *slot, format!("g{slot}")),
            Placement::Kept => g.clone(),
        })
        .collect();

    let combined: Vec<BoundAggregate> = aggregates
        .iter()
        .enumerate()
        .map(|(i, a)| BoundAggregate {
            // A count of rows becomes a sum of counts. Everything else
            // combines with itself.
            func: match a.func {
                AggregateFunction::Count => AggregateFunction::Sum,
                other => other,
            },
            // Reads the partial's output column rather than the original
            // argument, which no longer exists above the partial.
            arg: Some(BoundExpr {
                id: a.arg.as_ref().map_or(crate::plan::ExprId(0), |x| x.id),
                kind: BoundExprKind::Column {
                    rel: partial_rel,
                    index: side_groups.len() + i,
                    name: format!("p{i}"),
                },
                data_type: a.data_type,
                nullable: a.nullable,
                span: a.span,
            }),
            distinct: false,
            data_type: a.data_type,
            nullable: a.nullable,
            span: a.span,
        })
        .collect();

    Some(LogicalPlan::Aggregate {
        rel: agg_rel,
        group_exprs: new_groups,
        aggregates: combined,
        schema: Arc::clone(schema),
        input: Box::new(join),
    })
}

enum Placement {
    /// Computed by the partial, at this output position.
    Pushed(usize),
    /// Stays above the join, unchanged.
    Kept,
}

/// A column reference into the partial's output, carrying `template`'s type.
fn column_of(template: &BoundExpr, rel: RelId, index: usize, name: String) -> BoundExpr {
    BoundExpr {
        id: template.id,
        kind: BoundExprKind::Column { rel, index, name },
        data_type: template.data_type,
        nullable: template.nullable,
        span: template.span,
    }
}

fn same_column(a: &BoundExpr, b: &BoundExpr) -> bool {
    match (&a.kind, &b.kind) {
        (
            BoundExprKind::Column { rel: r1, index: i1, .. },
            BoundExprKind::Column { rel: r2, index: i2, .. },
        ) => r1 == r2 && i1 == i2,
        _ => false,
    }
}

/// Replace the mapped columns with references into the partial. Returns `None`
/// if the expression reads a column of the pushed side that is not in the map,
/// since that column no longer exists above the partial.
fn rewrite_columns(
    e: &BoundExpr,
    partial: RelId,
    mapping: &[(BoundExpr, usize)],
) -> Option<BoundExpr> {
    if let Some((_, slot)) = mapping.iter().find(|(c, _)| same_column(c, e)) {
        return Some(column_of(e, partial, *slot, format!("g{slot}")));
    }
    match &e.kind {
        BoundExprKind::Binary { op, left, right } => Some(BoundExpr {
            id: e.id,
            kind: BoundExprKind::Binary {
                op: *op,
                left: Box::new(rewrite_columns(left, partial, mapping)?),
                right: Box::new(rewrite_columns(right, partial, mapping)?),
            },
            data_type: e.data_type,
            nullable: e.nullable,
            span: e.span,
        }),
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => Some(e.clone()),
        // The condition is a conjunction of column equalities by construction,
        // so nothing else should appear.
        _ => None,
    }
}
