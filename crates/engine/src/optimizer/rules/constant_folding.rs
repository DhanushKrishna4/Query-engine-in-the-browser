//! Constant folding and boolean simplification.
//!
//! Evaluates any subexpression that reads no columns and replaces it with its
//! value, then simplifies the boolean structure that folding exposes -- which
//! is what turns `WHERE 1 = 1 AND x > 5` into `WHERE x > 5` and lets predicate
//! pushdown move something worth moving.
//!
//! Two things it deliberately does not do.
//!
//! **It never folds an expression that could raise.** `CASE WHEN false THEN 1/0
//! ELSE 2 END` is a perfectly good query; folding the dead branch would turn it
//! into a planning error. Folding is attempted and simply abandoned when
//! evaluation returns an error, which handles every case without enumerating
//! them.
//!
//! **It never drops an operand that could raise.** `x AND false` is `false`
//! only if evaluating `x` could not have failed; otherwise the rewrite would
//! turn a query that errors into one that does not. `BoundExpr::can_raise`
//! gates that.

use std::sync::Arc;

use crate::expr::{scalar, EvalContext};
use crate::optimizer::{OptimizerContext, Rule};
use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan};
use crate::storage::{Batch, Schema};
use crate::types::ScalarValue;

pub struct ConstantFolding;

impl Rule for ConstantFolding {
    fn name(&self) -> &'static str {
        "constant_folding"
    }

    fn apply(&self, plan: &LogicalPlan, _ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        // A filter whose predicate folded to a constant is not a filter any
        // more. This is what makes folding pay: an uncorrelated EXISTS becomes
        // TRUE and the whole operator disappears, and a contradiction becomes
        // FALSE and the scan below it never runs.
        if let LogicalPlan::Filter { predicate, input, .. } = plan {
            match constant_predicate(predicate) {
                Some(true) => return Some((**input).clone()),
                Some(false) => {
                    return Some(LogicalPlan::Limit {
                        rel: plan.rel(),
                        skip: 0,
                        // Nothing can match, so the input is never pulled at
                        // all -- `LimitExec` returns before asking for a batch.
                        fetch: Some(0),
                        input: input.clone(),
                    })
                }
                None => {}
            }
        }

        let mut changed = false;
        let folded = match plan {
            LogicalPlan::Filter { rel, predicate, input } => LogicalPlan::Filter {
                rel: *rel,
                predicate: fold(predicate, &mut changed),
                input: input.clone(),
            },
            LogicalPlan::Project { rel, exprs, schema, input } => LogicalPlan::Project {
                rel: *rel,
                exprs: exprs.iter().map(|e| fold(e, &mut changed)).collect(),
                schema: Arc::clone(schema),
                input: input.clone(),
            },
            LogicalPlan::Join { rel, join_type, on, left, right, schema } => LogicalPlan::Join {
                rel: *rel,
                join_type: *join_type,
                on: on.as_ref().map(|c| fold(c, &mut changed)),
                left: left.clone(),
                right: right.clone(),
                schema: Arc::clone(schema),
            },
            LogicalPlan::Aggregate { rel, group_exprs, aggregates, schema, input } => {
                LogicalPlan::Aggregate {
                    rel: *rel,
                    group_exprs: group_exprs.iter().map(|e| fold(e, &mut changed)).collect(),
                    aggregates: aggregates
                        .iter()
                        .map(|a| {
                            let mut a = a.clone();
                            a.arg = a.arg.as_ref().map(|e| fold(e, &mut changed));
                            a
                        })
                        .collect(),
                    schema: Arc::clone(schema),
                    input: input.clone(),
                }
            }
            _ => return None,
        };
        changed.then_some(folded)
    }
}

/// Fold an expression bottom-up. `changed` is set if anything was rewritten.
pub fn fold(e: &BoundExpr, changed: &mut bool) -> BoundExpr {
    let rebuilt = fold_children(e, changed);

    if let Some(simplified) = simplify(&rebuilt) {
        *changed = true;
        return simplified;
    }

    // A subexpression that reads nothing can be replaced by its value -- unless
    // evaluating it fails, in which case it has to stay so the failure happens
    // (or does not) at the same point it would have.
    if !matches!(rebuilt.kind, BoundExprKind::Literal(_)) && rebuilt.relation_set().is_empty() {
        if let Some(value) = try_evaluate(&rebuilt) {
            *changed = true;
            return BoundExpr {
                id: rebuilt.id,
                nullable: value.is_null(),
                kind: BoundExprKind::Literal(value),
                data_type: rebuilt.data_type,
                span: rebuilt.span,
            };
        }
    }
    rebuilt
}

fn try_evaluate(e: &BoundExpr) -> Option<ScalarValue> {
    let batch = Batch::empty_rows(Arc::new(Schema::empty()), 1);
    scalar::eval(e, &batch, 0, &EvalContext::empty()).ok()
}

/// Boolean identities. Each returns `None` when it does not apply.
fn simplify(e: &BoundExpr) -> Option<BoundExpr> {
    match &e.kind {
        BoundExprKind::Binary { op: BinaryOperator::And, left, right } => {
            match (as_bool(left), as_bool(right)) {
                // TRUE AND x is x.
                (Some(true), _) => Some((**right).clone()),
                (_, Some(true)) => Some((**left).clone()),
                // FALSE AND x is FALSE -- but only if x could not have raised.
                (Some(false), _) if !right.can_raise() => Some(literal_bool(e, false)),
                (_, Some(false)) if !left.can_raise() => Some(literal_bool(e, false)),
                _ => None,
            }
        }
        BoundExprKind::Binary { op: BinaryOperator::Or, left, right } => {
            match (as_bool(left), as_bool(right)) {
                (Some(false), _) => Some((**right).clone()),
                (_, Some(false)) => Some((**left).clone()),
                (Some(true), _) if !right.can_raise() => Some(literal_bool(e, true)),
                (_, Some(true)) if !left.can_raise() => Some(literal_bool(e, true)),
                _ => None,
            }
        }
        BoundExprKind::Unary { op: UnaryOperator::Not, expr } => match &expr.kind {
            // NOT NOT x is x, NULLs included.
            BoundExprKind::Unary { op: UnaryOperator::Not, expr: inner } => {
                Some((**inner).clone())
            }
            _ => None,
        },
        _ => None,
    }
}

/// A predicate that is a constant: `TRUE` passes everything, `FALSE` and NULL
/// pass nothing.
fn constant_predicate(e: &BoundExpr) -> Option<bool> {
    match &e.kind {
        BoundExprKind::Literal(ScalarValue::Boolean(b)) => Some(*b),
        BoundExprKind::Literal(ScalarValue::Null) => Some(false),
        _ => None,
    }
}

fn as_bool(e: &BoundExpr) -> Option<bool> {
    match &e.kind {
        BoundExprKind::Literal(ScalarValue::Boolean(b)) => Some(*b),
        _ => None,
    }
}

fn literal_bool(from: &BoundExpr, value: bool) -> BoundExpr {
    BoundExpr {
        id: from.id,
        kind: BoundExprKind::Literal(ScalarValue::Boolean(value)),
        data_type: crate::types::DataType::Boolean,
        nullable: false,
        span: from.span,
    }
}

/// Rebuild an expression with each child folded.
fn fold_children(e: &BoundExpr, changed: &mut bool) -> BoundExpr {
    let kind = match &e.kind {
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => return e.clone(),
        BoundExprKind::Binary { op, left, right } => BoundExprKind::Binary {
            op: *op,
            left: Box::new(fold(left, changed)),
            right: Box::new(fold(right, changed)),
        },
        BoundExprKind::Unary { op, expr } => BoundExprKind::Unary {
            op: *op,
            expr: Box::new(fold(expr, changed)),
        },
        BoundExprKind::Cast { expr, implicit } => BoundExprKind::Cast {
            expr: Box::new(fold(expr, changed)),
            implicit: *implicit,
        },
        BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
            expr: Box::new(fold(expr, changed)),
            negated: *negated,
        },
        BoundExprKind::Between { expr, low, high, negated } => BoundExprKind::Between {
            expr: Box::new(fold(expr, changed)),
            low: Box::new(fold(low, changed)),
            high: Box::new(fold(high, changed)),
            negated: *negated,
        },
        BoundExprKind::InList { expr, list, negated } => BoundExprKind::InList {
            expr: Box::new(fold(expr, changed)),
            list: list.iter().map(|i| fold(i, changed)).collect(),
            negated: *negated,
        },
        BoundExprKind::Like { expr, pattern, escape, negated, case_insensitive } => {
            BoundExprKind::Like {
                expr: Box::new(fold(expr, changed)),
                pattern: Box::new(fold(pattern, changed)),
                escape: escape.as_ref().map(|x| Box::new(fold(x, changed))),
                negated: *negated,
                case_insensitive: *case_insensitive,
            }
        }
        // Left alone: a subquery is resolved by its own pass, and folding
        // would have to evaluate it.
        BoundExprKind::Subquery { .. } => return e.clone(),
        BoundExprKind::Case { when_then, else_expr } => BoundExprKind::Case {
            when_then: when_then
                .iter()
                .map(|(w, t)| (fold(w, changed), fold(t, changed)))
                .collect(),
            else_expr: else_expr.as_ref().map(|x| Box::new(fold(x, changed))),
        },
    };
    BoundExpr { kind, ..e.clone() }
}
