//! Turning subqueries into things the executor can run.
//!
//! Nothing downstream of the optimizer can evaluate a subquery expression, so
//! one of these two rules has to remove every one of them. They run even when
//! the optimizer is otherwise disabled.
//!
//! [`Decorrelate`] handles `EXISTS` and `IN` in a WHERE clause by turning them
//! into semi- and anti-joins. This is the rewrite worth watching: a correlated
//! `EXISTS` -- conceptually "for each outer row, run this query" -- collapses
//! into a single join, and the quadratic disappears.
//!
//! [`EvaluateSubqueries`] handles everything left over by *running* it. That is
//! only possible when the subquery is uncorrelated, since a correlated one has
//! a different answer per outer row.
//!
//! ## When a join is the wrong answer
//!
//! For an *uncorrelated* subquery both routes work, and which is faster depends
//! on how much comes back. Measured on a million rows: turning
//! `IN (SELECT ...)` returning thousands of values into a semi-join is 115x
//! faster than comparing every row against every value, while doing the same to
//! one returning two values is 2.4x *slower* -- a hash build and probe costs
//! more than two vectorized comparisons.
//!
//! So an uncorrelated `IN` is only decorrelated when the subquery is estimated
//! to return more than [`MATERIALIZE_THRESHOLD`] rows, and an uncorrelated
//! `EXISTS` never is: it has nothing to join on and folds to a constant.
//! A correlated subquery is always decorrelated, because the alternative is
//! that it does not run at all.
//!
//! ## The NULL rule that makes `NOT IN` different from `NOT EXISTS`
//!
//! `NOT EXISTS` is about whether any row came back, so it is never unknown and
//! always becomes an anti-join.
//!
//! `NOT IN` is a comparison, and a NULL on either side makes it unknown rather
//! than true. `x NOT IN (SELECT y ...)` where some `y` is NULL matches *no*
//! rows at all -- not "every row where x differs". An anti-join would return
//! those rows, so `NOT IN` is only rewritten when neither side can be NULL.
//! Otherwise it falls through to [`EvaluateSubqueries`], which materializes the
//! values and applies the three-valued rule directly.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::Diagnostic;
use crate::exec;
use crate::optimizer::{Optimizer, OptimizerContext, Rule};
use crate::parser::ast::BinaryOperator;
use crate::plan::{
    join_schema, BoundExpr, BoundExprKind, JoinType, LogicalPlan, RelId, SubqueryKind,
};
use crate::types::{DataType, ScalarValue};

// ---------------------------------------------------------------------------
// EXISTS / IN in a WHERE clause -> semi / anti join
// ---------------------------------------------------------------------------

pub struct Decorrelate;

impl Rule for Decorrelate {
    fn name(&self) -> &'static str {
        "decorrelate"
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Filter { rel, predicate, input } = plan else {
            return None;
        };

        let mut conjuncts = Vec::new();
        predicate.clone().split_conjuncts(&mut conjuncts);

        // One conjunct per application, so each rewrite is its own trace step.
        let position = conjuncts.iter().position(|c| is_rewritable(c, ctx))?;
        let subquery = conjuncts.remove(position);
        let BoundExprKind::Subquery { kind, plan: subplan } = &subquery.kind else {
            return None;
        };

        let outer = input.defined_relations();
        let join = build_semi_join(kind, subplan, input, &outer, ctx)?;

        Some(match BoundExpr::and_all(conjuncts) {
            Some(remaining) => LogicalPlan::Filter {
                rel: *rel,
                predicate: remaining,
                input: Box::new(join),
            },
            None => join,
        })
    }
}

/// Estimated subquery size above which a semi-join beats materializing the
/// values. Hand-picked from the benchmark, where the crossover sits somewhere
/// in the low tens; the exact value matters far less than not being at either
/// extreme.
pub const MATERIALIZE_THRESHOLD: f64 = 16.0;

/// Whether a conjunct is a subquery this rule should turn into a join.
fn is_rewritable(e: &BoundExpr, ctx: &OptimizerContext) -> bool {
    let BoundExprKind::Subquery { kind, plan } = &e.kind else {
        return false;
    };
    let correlated = plan.is_correlated();
    match kind {
        SubqueryKind::Scalar => false,

        // An uncorrelated EXISTS has nothing to join on: it is a constant, and
        // evaluating it once is strictly better than a cross semi-join.
        SubqueryKind::Exists { .. } => correlated,

        SubqueryKind::In { expr, negated } => {
            // See the module note: NOT IN is only an anti-join when no NULL can
            // reach either side.
            if *negated && (expr.nullable || plan.schema().fields.iter().any(|f| f.nullable)) {
                return false;
            }
            correlated
                || crate::optimizer::stats::estimate_rows(plan, ctx.catalog)
                    > MATERIALIZE_THRESHOLD
        }
    }
}

fn build_semi_join(
    kind: &SubqueryKind,
    subplan: &Arc<LogicalPlan>,
    input: &LogicalPlan,
    outer: &BTreeSet<RelId>,
    ctx: &mut OptimizerContext,
) -> Option<LogicalPlan> {
    let negated = match kind {
        SubqueryKind::Exists { negated } => *negated,
        SubqueryKind::In { negated, .. } => *negated,
        SubqueryKind::Scalar => return None,
    };
    let join_type = if negated {
        JoinType::Anti
    } else {
        JoinType::Semi
    };

    // Strip the subquery's projection. EXISTS does not care what it selected,
    // and IN needs the expression itself rather than a column of an anonymous
    // relation -- and without the projection in the way, the subquery's own
    // relations stay addressable from the join condition.
    let (body, projected) = match &**subplan {
        LogicalPlan::Project { exprs, input, .. } => (input.as_ref().clone(), exprs.clone()),
        other => (other.clone(), Vec::new()),
    };

    // Pulling a correlated predicate out from under an aggregate or a limit
    // changes what it is filtering, so those shapes are left alone.
    if body.is_correlated() && contains_barrier(&body) {
        return None;
    }

    let (body, mut conditions) = extract_correlated(&body, outer);
    if body.is_correlated() {
        // A correlated reference somewhere this rule cannot lift it out of.
        return None;
    }

    if let SubqueryKind::In { expr, .. } = kind {
        let value = projected.first()?.clone();
        if !value.relation_set().is_disjoint(outer) {
            // The projected expression itself reads an outer column.
            return None;
        }
        conditions.push(equals(expr, &value));
    }

    let on = BoundExpr::and_all(conditions);
    let left = input.clone();
    let schema = join_schema(join_type, &left, &body);
    Some(LogicalPlan::Join {
        rel: ctx.ids.allocate(),
        join_type,
        on,
        left: Box::new(left),
        right: Box::new(body),
        schema,
    })
}

/// Whether the plan contains an operator a filter cannot be lifted through.
fn contains_barrier(plan: &LogicalPlan) -> bool {
    if matches!(
        plan,
        LogicalPlan::Aggregate { .. } | LogicalPlan::Limit { .. }
    ) {
        return true;
    }
    plan.children().iter().any(|c| contains_barrier(c))
}

/// Lift predicates that mention the outer query out of the subquery, returning
/// the remainder and the lifted conditions.
///
/// This is the actual decorrelation: `WHERE b.x = a.y` inside the subquery
/// becomes the join's `ON b.x = a.y`, which is what lets one join replace a
/// per-row re-execution.
fn extract_correlated(
    plan: &LogicalPlan,
    outer: &BTreeSet<RelId>,
) -> (LogicalPlan, Vec<BoundExpr>) {
    match plan {
        LogicalPlan::Filter { rel, predicate, input } => {
            let (input, mut lifted) = extract_correlated(input, outer);
            let mut conjuncts = Vec::new();
            predicate.clone().split_conjuncts(&mut conjuncts);

            let mut stay = Vec::new();
            for c in conjuncts {
                if c.relation_set().is_disjoint(outer) {
                    stay.push(c);
                } else {
                    lifted.push(c);
                }
            }
            let plan = match BoundExpr::and_all(stay) {
                Some(predicate) => LogicalPlan::Filter {
                    rel: *rel,
                    predicate,
                    input: Box::new(input),
                },
                None => input,
            };
            (plan, lifted)
        }
        other => {
            let mut lifted = Vec::new();
            let children: Vec<LogicalPlan> = other
                .children()
                .into_iter()
                .map(|c| {
                    let (c, mut l) = extract_correlated(c, outer);
                    lifted.append(&mut l);
                    c
                })
                .collect();
            (other.with_children(children), lifted)
        }
    }
}

fn equals(left: &BoundExpr, right: &BoundExpr) -> BoundExpr {
    BoundExpr {
        id: left.id,
        data_type: DataType::Boolean,
        nullable: left.nullable || right.nullable,
        span: left.span.merge(right.span),
        kind: BoundExprKind::Binary {
            op: BinaryOperator::Eq,
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        },
    }
}

// ---------------------------------------------------------------------------
// Everything else: run it
// ---------------------------------------------------------------------------

pub struct EvaluateSubqueries;

impl Rule for EvaluateSubqueries {
    fn name(&self) -> &'static str {
        "evaluate_subqueries"
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let mut changed = false;
        let rewritten = plan.rewrite_expressions(&mut |e| {
            if changed || ctx.error.is_some() {
                return None;
            }
            let BoundExprKind::Subquery { kind, plan } = &e.kind else {
                return None;
            };
            if plan.is_correlated() {
                // Nothing can run this: the answer differs per outer row and
                // `Decorrelate` did not recognise the shape.
                ctx.error = Some(
                    Diagnostic::bind(
                        format!(
                            "this correlated {} subquery cannot be rewritten as a join",
                            kind.label()
                        ),
                        e.span,
                    )
                    .with_hint(
                        "correlated subqueries are supported as EXISTS or IN in a WHERE clause, \
                         over a plan with no GROUP BY or LIMIT",
                    ),
                );
                return None;
            }
            match evaluate(&kind.clone(), plan, e, ctx) {
                Ok(folded) => {
                    changed = true;
                    Some(folded)
                }
                Err(d) => {
                    ctx.error = Some(d);
                    None
                }
            }
        });
        changed.then_some(rewritten)
    }
}

/// Run an uncorrelated subquery and fold it into a value.
fn evaluate(
    kind: &SubqueryKind,
    plan: &Arc<LogicalPlan>,
    expr: &BoundExpr,
    ctx: &mut OptimizerContext,
) -> Result<BoundExpr, Diagnostic> {
    // Optimize the subquery first, which also resolves any subqueries nested
    // inside it -- the rule driver walks a plan's children, and a subquery's
    // plan is not one of them.
    let optimized = Optimizer::new().optimize((**plan).clone(), ctx.catalog);
    let mut root = exec::build_with(&optimized, ctx.catalog, &exec::ExecOptions::default())?;
    let batches = exec::collect(root.as_mut())?;

    let mut values: Vec<ScalarValue> = Vec::new();
    let mut rows = 0usize;
    for batch in &batches {
        rows += batch.num_rows();
        for row in 0..batch.num_rows() {
            if !batch.columns().is_empty() {
                values.push(batch.value(0, row));
            }
        }
    }

    let literal = |value: ScalarValue, data_type: DataType| BoundExpr {
        id: expr.id,
        nullable: value.is_null(),
        kind: BoundExprKind::Literal(value),
        data_type,
        span: expr.span,
    };

    Ok(match kind {
        SubqueryKind::Scalar => match rows {
            0 => literal(ScalarValue::Null, expr.data_type),
            1 => literal(values.remove(0), expr.data_type),
            n => {
                return Err(Diagnostic::exec(format!(
                    "a subquery used as a value returned {n} rows, but must return at most one"
                ))
                .with_span(expr.span))
            }
        },

        SubqueryKind::Exists { negated } => {
            // EXISTS never yields NULL: it asks whether a row came back.
            literal(ScalarValue::Boolean((rows > 0) != *negated), DataType::Boolean)
        }

        SubqueryKind::In { expr: value, negated } => {
            // Becomes an ordinary IN list, which already implements the
            // three-valued rule -- including a NULL in the list making an
            // otherwise-false `NOT IN` unknown.
            let list: Vec<BoundExpr> = values
                .into_iter()
                .map(|v| {
                    let dt = if v.is_null() { value.data_type } else { v.data_type() };
                    literal(v, dt)
                })
                .collect();
            BoundExpr {
                id: expr.id,
                kind: BoundExprKind::InList {
                    expr: value.clone(),
                    list,
                    negated: *negated,
                },
                data_type: DataType::Boolean,
                nullable: expr.nullable,
                span: expr.span,
            }
        }
    })
}
