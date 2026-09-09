//! Predicate simplification and contradiction detection.
//!
//! Two rewrites over the conjuncts of a filter, both driven by the same idea:
//! a chain of comparisons against literals on one column describes an
//! *interval*, and an interval is a thing you can reason about.
//!
//! ```text
//!   x > 5 AND x > 3   ->   x > 5          the weaker bound says nothing new
//!   x > 5 AND x < 3   ->   false          the interval is empty
//!   x = 4 AND x > 9   ->   false          so is this one
//! ```
//!
//! ## Why this is safe under three-valued logic
//!
//! Dropping `x > 3` because `x > 5` is present looks like it might change what
//! happens to a NULL `x`, and it does not. With `x` NULL both comparisons are
//! UNKNOWN, so the conjunction is UNKNOWN before the rewrite and UNKNOWN after;
//! the row fails the filter either way. For non-NULL `x` the implication is
//! ordinary arithmetic: anything above 5 is above 3, so the second test can add
//! nothing to the first.
//!
//! Contradiction is the same argument. `x > 5 AND x < 3` is FALSE for every
//! non-NULL `x` and UNKNOWN for NULL, and a filter keeps a row only when its
//! predicate is TRUE -- so no row survives, and `false` rejects exactly the
//! same rows. It is *not* generally true that UNKNOWN may be replaced by FALSE;
//! it is true here because the only consumer is a filter, which treats them
//! identically. An `ON` clause of an outer join would too, for the same reason:
//! neither UNKNOWN nor FALSE is a match, so the padding happens either way.
//!
//! ## What it does not do
//!
//! A contradiction becomes the literal `false` rather than a plan node that
//! produces nothing. `false` costs a scan that prunes every row group on its
//! zone map -- so no data is read -- but the operators are still built and
//! still asked for a batch. An `Empty` plan node would let the planner skip
//! that, and there is not one.
//!
//! Only bare `column <op> literal` conjuncts are reasoned about. `x + 1 > 5`
//! describes an interval too, and recognising it means normalizing arithmetic,
//! which is a different rule.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::optimizer::{OptimizerContext, Rule};
use crate::parser::ast::BinaryOperator;
use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan, RelId};
use crate::types::{self, DataType, ScalarValue};

pub struct PredicateSimplification;

/// One end of the range a column is constrained to.
#[derive(Clone)]
struct Bound {
    value: ScalarValue,
    /// `>=` and `<=` include their endpoint; `>` and `<` do not.
    inclusive: bool,
    /// Which conjunct produced it, so the survivors can be kept and the rest
    /// dropped.
    from: usize,
}

#[derive(Default)]
struct Interval {
    lower: Option<Bound>,
    upper: Option<Bound>,
}

impl Interval {
    /// Keep the tighter of two lower bounds. `>` beats `>=` at the same value,
    /// because it admits strictly less.
    fn tighten_lower(&mut self, candidate: Bound) {
        let replace = match &self.lower {
            None => true,
            Some(current) => match types::compare(&candidate.value, &current.value) {
                Some(Ordering::Greater) => true,
                Some(Ordering::Equal) => !candidate.inclusive && current.inclusive,
                _ => false,
            },
        };
        if replace {
            self.lower = Some(candidate);
        }
    }

    fn tighten_upper(&mut self, candidate: Bound) {
        let replace = match &self.upper {
            None => true,
            Some(current) => match types::compare(&candidate.value, &current.value) {
                Some(Ordering::Less) => true,
                Some(Ordering::Equal) => !candidate.inclusive && current.inclusive,
                _ => false,
            },
        };
        if replace {
            self.upper = Some(candidate);
        }
    }

    /// Whether no value can satisfy both ends.
    fn is_empty(&self) -> bool {
        let (Some(lo), Some(hi)) = (&self.lower, &self.upper) else {
            return false;
        };
        match types::compare(&lo.value, &hi.value) {
            Some(Ordering::Greater) => true,
            // `x >= 5 AND x <= 5` is satisfiable by 5; `x > 5 AND x <= 5` is not.
            Some(Ordering::Equal) => !(lo.inclusive && hi.inclusive),
            _ => false,
        }
    }
}

impl Rule for PredicateSimplification {
    fn name(&self) -> &'static str {
        "predicate_simplification"
    }

    fn apply(&self, plan: &LogicalPlan, _ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Filter { rel, predicate, input } = plan else {
            return None;
        };

        let mut conjuncts = Vec::new();
        predicate.clone().split_conjuncts(&mut conjuncts);
        if conjuncts.len() < 2 {
            return None;
        }

        // Gather the interval each column is confined to, remembering which
        // conjunct contributed each surviving bound.
        //
        // `contributed` is what makes dropping safe. A conjunct may only be
        // dropped if it *produced* a bound that something tighter then
        // superseded. Treating every recognised comparison as droppable is
        // wrong and was: `<>` is a comparison and constrains no interval, so
        // `HAVING SUM(q) >= 4 AND product <> 'cable'` quietly lost its second
        // test. The corpus caught it.
        let mut intervals: BTreeMap<(RelId, usize), Interval> = BTreeMap::new();
        let mut contributed = vec![false; conjuncts.len()];
        for (i, c) in conjuncts.iter().enumerate() {
            // A conjunct that could raise must be kept even if it says nothing
            // new: dropping it would turn a query that errors into one that
            // does not. `column <op> literal` never can, but the check is
            // cheap and the rule is about not being clever in the wrong place.
            if c.can_raise() {
                continue;
            }
            let Some((column, op, value)) = comparison(c) else {
                continue;
            };
            let entry = intervals.entry(column).or_default();
            match op {
                BinaryOperator::Gt => entry.tighten_lower(Bound { value, inclusive: false, from: i }),
                BinaryOperator::GtEq => entry.tighten_lower(Bound { value, inclusive: true, from: i }),
                BinaryOperator::Lt => entry.tighten_upper(Bound { value, inclusive: false, from: i }),
                BinaryOperator::LtEq => entry.tighten_upper(Bound { value, inclusive: true, from: i }),
                BinaryOperator::Eq => {
                    entry.tighten_lower(Bound { value: value.clone(), inclusive: true, from: i });
                    entry.tighten_upper(Bound { value, inclusive: true, from: i });
                }
                // `<>` and the rest constrain no interval. They stay.
                _ => continue,
            }
            contributed[i] = true;
        }

        if intervals.values().any(Interval::is_empty) {
            // Nothing can satisfy this. `false` prunes every row group on its
            // zone map, so the scan reads nothing.
            return Some(LogicalPlan::Filter {
                rel: *rel,
                predicate: literal_false(predicate),
                input: input.clone(),
            });
        }

        // Keep everything that did not produce a bound, plus exactly the
        // bounds that survived tightening.
        let mut keep: Vec<bool> = contributed.iter().map(|c| !c).collect();
        for interval in intervals.values() {
            for bound in [&interval.lower, &interval.upper].into_iter().flatten() {
                keep[bound.from] = true;
            }
        }
        if keep.iter().all(|k| *k) {
            return None;
        }

        let kept: Vec<BoundExpr> = conjuncts
            .into_iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(c, _)| c)
            .collect();
        Some(LogicalPlan::Filter {
            rel: *rel,
            predicate: conjoin(kept, predicate),
            input: input.clone(),
        })
    }
}

/// A `column <op> literal` comparison, with the operator oriented so the column
/// is on the left. `5 < x` is `x > 5`.
fn comparison(e: &BoundExpr) -> Option<((RelId, usize), BinaryOperator, ScalarValue)> {
    let BoundExprKind::Binary { op, left, right } = &e.kind else {
        return None;
    };
    if !op.is_comparison() {
        return None;
    }
    // A NULL literal makes the comparison UNKNOWN for every row; that is not an
    // interval and reasoning about it would be wrong.
    let column = |e: &BoundExpr| match &e.kind {
        BoundExprKind::Column { rel, index, .. } => Some((*rel, *index)),
        _ => None,
    };
    let literal = |e: &BoundExpr| match &e.kind {
        BoundExprKind::Literal(v) if !v.is_null() => Some(v.clone()),
        _ => None,
    };

    if let (Some(c), Some(v)) = (column(left), literal(right)) {
        return Some((c, *op, v));
    }
    if let (Some(v), Some(c)) = (literal(left), column(right)) {
        return Some((c, flip(*op), v));
    }
    None
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

/// Rebuild an `AND` chain, reusing the original's ids and span so the plan
/// keeps pointing at the SQL the user wrote.
fn conjoin(mut parts: Vec<BoundExpr>, original: &BoundExpr) -> BoundExpr {
    let Some(mut acc) = parts.pop() else {
        return literal_true(original);
    };
    while let Some(next) = parts.pop() {
        acc = BoundExpr {
            id: original.id,
            kind: BoundExprKind::Binary {
                op: BinaryOperator::And,
                left: Box::new(next),
                right: Box::new(acc),
            },
            data_type: DataType::Boolean,
            nullable: true,
            span: original.span,
        };
    }
    acc
}

fn literal_false(original: &BoundExpr) -> BoundExpr {
    BoundExpr {
        id: original.id,
        kind: BoundExprKind::Literal(ScalarValue::Boolean(false)),
        data_type: DataType::Boolean,
        nullable: false,
        span: original.span,
    }
}

fn literal_true(original: &BoundExpr) -> BoundExpr {
    BoundExpr {
        id: original.id,
        kind: BoundExprKind::Literal(ScalarValue::Boolean(true)),
        data_type: DataType::Boolean,
        nullable: false,
        span: original.span,
    }
}
