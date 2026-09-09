//! Predicate pushdown.
//!
//! Moves each conjunct of a filter as far down the plan as it can legally go.
//! Below a join it becomes a filter on one input, so the join sees fewer rows;
//! below an aggregate it becomes a WHERE, so fewer rows are grouped; and a
//! filter that lands directly on a scan is what feeds zone-map pruning.
//!
//! ## The outer-join rule, and why
//!
//! A predicate may only be pushed into a side the join **preserves**.
//!
//! Consider `a LEFT JOIN b ON a.id = b.id WHERE b.y > 5`. Filtering `b` before
//! the join removes its low-`y` rows, so some `a` rows that would have matched
//! now find nothing -- and a LEFT join emits those, padded with NULLs. Since
//! the predicate moved down, nothing above removes them any more, and the query
//! returns rows it should not have.
//!
//! Pushing the same predicate into the *preserved* side is fine:
//! `WHERE a.x > 5` removes `a` rows that the filter above would have removed
//! anyway, matched or padded.
//!
//! So: inner and cross joins take predicates on either side; a LEFT join takes
//! them only on the left; a RIGHT join only on the right; a FULL join takes
//! none. (The rewrite that turns an outer join into an inner one when a
//! downstream predicate rejects NULLs is what unlocks the other direction, and
//! it is a separate rule.)

use std::sync::Arc;

use crate::optimizer::{OptimizerContext, Rule};
use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan};

pub struct PredicatePushdown;

impl Rule for PredicatePushdown {
    fn name(&self) -> &'static str {
        "predicate_pushdown"
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Filter { rel, predicate, input } = plan else {
            return None;
        };

        match &**input {
            // Two filters in a row are one filter.
            LogicalPlan::Filter { predicate: inner, input: below, .. } => {
                let mut parts = Vec::new();
                predicate.clone().split_conjuncts(&mut parts);
                inner.clone().split_conjuncts(&mut parts);
                Some(LogicalPlan::Filter {
                    rel: *rel,
                    predicate: BoundExpr::and_all(parts)?,
                    input: below.clone(),
                })
            }

            LogicalPlan::Join { .. } => push_into_join(*rel, predicate, input, ctx),

            LogicalPlan::Aggregate { .. } => push_into_aggregate(*rel, predicate, input, ctx),

            _ => None,
        }
    }
}

fn push_into_join(
    filter_rel: crate::plan::RelId,
    predicate: &BoundExpr,
    join: &LogicalPlan,
    ctx: &mut OptimizerContext,
) -> Option<LogicalPlan> {
    let LogicalPlan::Join { rel, join_type, on, left, right, .. } = join else {
        return None;
    };

    let left_rels = left.relations();
    let right_rels = right.relations();

    let mut conjuncts = Vec::new();
    predicate.clone().split_conjuncts(&mut conjuncts);

    let (mut to_left, mut to_right, mut stay) = (Vec::new(), Vec::new(), Vec::new());
    for c in conjuncts {
        let rels = c.relation_set();
        let only_left = !rels.is_empty() && rels.is_subset(&left_rels);
        let only_right = !rels.is_empty() && rels.is_subset(&right_rels);

        // `preserves_right` means the *left* side can be NULL-padded, so the
        // left side is the null-supplying one and must not be filtered.
        if only_left && !join_type.preserves_right() {
            to_left.push(c);
        } else if only_right && !join_type.preserves_left() {
            to_right.push(c);
        } else {
            stay.push(c);
        }
    }

    if to_left.is_empty() && to_right.is_empty() {
        return None;
    }

    let new_left = wrap(left, to_left, ctx);
    let new_right = wrap(right, to_right, ctx);
    let new_join = LogicalPlan::Join {
        rel: *rel,
        join_type: *join_type,
        on: on.clone(),
        schema: crate::plan::join_schema(*join_type, &new_left, &new_right),
        left: Box::new(new_left),
        right: Box::new(new_right),
    };

    Some(match BoundExpr::and_all(stay) {
        Some(remaining) => LogicalPlan::Filter {
            rel: filter_rel,
            predicate: remaining,
            input: Box::new(new_join),
        },
        // Everything moved, so the filter itself is gone.
        None => new_join,
    })
}

/// Push the parts of a HAVING clause that only mention grouping keys below the
/// aggregate, where they become an ordinary WHERE.
///
/// This is valid precisely because a grouping key has the same value for every
/// row of its group: filtering on it before grouping removes exactly the rows
/// that would have formed the groups the HAVING was about to discard. A
/// condition mentioning an *aggregate* has no such property and stays put.
fn push_into_aggregate(
    filter_rel: crate::plan::RelId,
    predicate: &BoundExpr,
    aggregate: &LogicalPlan,
    ctx: &mut OptimizerContext,
) -> Option<LogicalPlan> {
    let LogicalPlan::Aggregate { rel, group_exprs, aggregates, schema, input } = aggregate else {
        return None;
    };
    if group_exprs.is_empty() {
        return None;
    }

    let mut conjuncts = Vec::new();
    predicate.clone().split_conjuncts(&mut conjuncts);

    let (mut push, mut stay) = (Vec::new(), Vec::new());
    for c in conjuncts {
        match substitute_group_keys(&c, *rel, group_exprs) {
            Some(rewritten) => push.push(rewritten),
            None => stay.push(c),
        }
    }
    if push.is_empty() {
        return None;
    }

    let new_input = wrap(input, push, ctx);
    let new_aggregate = LogicalPlan::Aggregate {
        rel: *rel,
        group_exprs: group_exprs.clone(),
        aggregates: aggregates.clone(),
        schema: Arc::clone(schema),
        input: Box::new(new_input),
    };

    Some(match BoundExpr::and_all(stay) {
        Some(remaining) => LogicalPlan::Filter {
            rel: filter_rel,
            predicate: remaining,
            input: Box::new(new_aggregate),
        },
        None => new_aggregate,
    })
}

/// Replace references to the aggregate's grouping-key columns with the
/// expressions those keys were computed from, so the predicate can be evaluated
/// against the aggregate's *input*. Returns `None` if the predicate touches an
/// aggregate result, which cannot be moved.
fn substitute_group_keys(
    e: &BoundExpr,
    agg_rel: crate::plan::RelId,
    group_exprs: &[BoundExpr],
) -> Option<BoundExpr> {
    let kind = match &e.kind {
        BoundExprKind::Column { rel, index, .. } => {
            if *rel != agg_rel {
                return None;
            }
            // Indices past the grouping keys are aggregate results.
            return group_exprs.get(*index).cloned();
        }
        BoundExprKind::Literal(_) => return Some(e.clone()),
        BoundExprKind::Binary { op, left, right } => BoundExprKind::Binary {
            op: *op,
            left: Box::new(substitute_group_keys(left, agg_rel, group_exprs)?),
            right: Box::new(substitute_group_keys(right, agg_rel, group_exprs)?),
        },
        BoundExprKind::Unary { op, expr } => BoundExprKind::Unary {
            op: *op,
            expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
        },
        BoundExprKind::Cast { expr, implicit } => BoundExprKind::Cast {
            expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
            implicit: *implicit,
        },
        BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
            expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
            negated: *negated,
        },
        BoundExprKind::Between { expr, low, high, negated } => BoundExprKind::Between {
            expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
            low: Box::new(substitute_group_keys(low, agg_rel, group_exprs)?),
            high: Box::new(substitute_group_keys(high, agg_rel, group_exprs)?),
            negated: *negated,
        },
        BoundExprKind::InList { expr, list, negated } => BoundExprKind::InList {
            expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
            list: list
                .iter()
                .map(|i| substitute_group_keys(i, agg_rel, group_exprs))
                .collect::<Option<Vec<_>>>()?,
            negated: *negated,
        },
        BoundExprKind::Like { expr, pattern, escape, negated, case_insensitive } => {
            BoundExprKind::Like {
                expr: Box::new(substitute_group_keys(expr, agg_rel, group_exprs)?),
                pattern: Box::new(substitute_group_keys(pattern, agg_rel, group_exprs)?),
                escape: match escape {
                    Some(x) => Some(Box::new(substitute_group_keys(x, agg_rel, group_exprs)?)),
                    None => None,
                },
                negated: *negated,
                case_insensitive: *case_insensitive,
            }
        }
        // CASE is left alone: nothing is gained and the substitution has more
        // ways to be subtly wrong than it is worth. A subquery likewise -- its
        // plan would have to be rewritten too.
        BoundExprKind::Case { .. } | BoundExprKind::Subquery { .. } => return None,
    };
    Some(BoundExpr { kind, ..e.clone() })
}

/// Put `parts` in a filter above `plan`, or return `plan` unchanged if empty.
fn wrap(plan: &LogicalPlan, parts: Vec<BoundExpr>, ctx: &mut OptimizerContext) -> LogicalPlan {
    match BoundExpr::and_all(parts) {
        Some(predicate) => LogicalPlan::Filter {
            rel: ctx.ids.allocate(),
            predicate,
            input: Box::new(plan.clone()),
        },
        None => plan.clone(),
    }
}
