//! Outer join to inner join, when a predicate above rejects the padding.
//!
//! An outer join keeps rows that matched nothing and fills the other side with
//! NULLs. If a filter above then demands something of those NULL columns, every
//! padded row fails it -- so the padding was manufactured only to be thrown
//! away, and the join might as well have been an inner one.
//!
//! ```sql
//!   SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.x > 5
//! ```
//!
//! `b.x` is NULL in every padded row, `NULL > 5` is UNKNOWN, and the filter
//! keeps a row only when its predicate is TRUE. Not one padded row survives.
//! Rewriting to an inner join lets the planner build the smaller side, lets
//! predicate pushdown move `b.x > 5` down into `b`, and stops the join
//! producing rows the filter is about to delete.
//!
//! This is the mirror image of the pushdown rule's central caution. Predicate
//! pushdown may only push into the *preserved* side of an outer join; this rule
//! fires exactly when a predicate on the *null-supplying* side makes the
//! outerness pointless. The two rules are about the same asymmetry from
//! opposite ends.
//!
//! ## Null-rejection is the whole question
//!
//! A predicate is null-rejecting for a relation when it cannot be TRUE if that
//! relation's columns are all NULL. `b.x > 5` is; `b.x IS NULL` is emphatically
//! not, and neither is `b.x IS NULL OR a.y > 1`, which is exactly how you write
//! an anti-join in SQL. Getting this backwards deletes the rows the query was
//! written to find, so the test below is conservative: it recognises the
//! constructs it can prove and says no to everything else.
//!
//! `COALESCE(b.x, 0) > 5` is null-rejecting too and is not recognised, because
//! this engine has no `COALESCE`. When it does, it belongs here.

use crate::optimizer::{OptimizerContext, Rule};
use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind, JoinType, LogicalPlan, RelId};
use crate::types::ScalarValue;

pub struct OuterToInner;

impl Rule for OuterToInner {
    fn name(&self) -> &'static str {
        "outer_to_inner"
    }

    fn apply(&self, plan: &LogicalPlan, _ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Filter { rel, predicate, input } = plan else {
            return None;
        };
        let LogicalPlan::Join { rel: join_rel, join_type, on, left, right, .. } = &**input
        else {
            return None;
        };
        if !matches!(join_type, JoinType::Left | JoinType::Right | JoinType::Full) {
            return None;
        }

        // Which side's padding does the filter reject?
        let left_rels = left.relations();
        let right_rels = right.relations();
        let rejects_left = rejects_nulls_from(predicate, &left_rels);
        let rejects_right = rejects_nulls_from(predicate, &right_rels);

        // A LEFT join pads the *right*, so it becomes inner when the right's
        // NULLs are rejected. A FULL join needs both sides considered, and
        // rejecting one side's padding demotes it to the join that still
        // preserves the other.
        let demoted = match join_type {
            JoinType::Left if rejects_right => JoinType::Inner,
            JoinType::Right if rejects_left => JoinType::Inner,
            // The direction here is easy to get backwards, and being backwards
            // deletes rows. The question is always *which side's padding* the
            // predicate rejects, and padding on one side belongs to unmatched
            // rows of the other.
            //
            // Rejecting the right side's NULLs kills the rows padded on the
            // right -- which are the unmatched rows of the LEFT. So the left
            // no longer needs preserving, and what remains is a RIGHT join.
            JoinType::Full => match (rejects_left, rejects_right) {
                (true, true) => JoinType::Inner,
                (true, false) => JoinType::Left,
                (false, true) => JoinType::Right,
                (false, false) => return None,
            },
            _ => return None,
        };

        Some(LogicalPlan::Filter {
            rel: *rel,
            predicate: predicate.clone(),
            input: Box::new(LogicalPlan::Join {
                rel: *join_rel,
                join_type: demoted,
                on: on.clone(),
                left: left.clone(),
                right: right.clone(),
                schema: crate::plan::join_schema(demoted, left, right),
            }),
        })
    }
}

/// Whether `predicate` is guaranteed not to be TRUE when every column of
/// `rels` is NULL.
///
/// Conservative: an unrecognised construct answers `false`, which leaves the
/// join alone. The cost of that is a missed rewrite; the cost of the opposite
/// is dropped rows.
fn rejects_nulls_from(
    predicate: &BoundExpr,
    rels: &std::collections::BTreeSet<RelId>,
) -> bool {
    match &predicate.kind {
        // A conjunction rejects if *either* side does: the whole is TRUE only
        // when both are, so one UNKNOWN or FALSE is enough.
        BoundExprKind::Binary { op: BinaryOperator::And, left, right } => {
            rejects_nulls_from(left, rels) || rejects_nulls_from(right, rels)
        }
        // A disjunction rejects only if *both* sides do. `b.x > 5 OR a.y > 1`
        // can be TRUE on a padded row, so it rejects nothing.
        BoundExprKind::Binary { op: BinaryOperator::Or, left, right } => {
            rejects_nulls_from(left, rels) && rejects_nulls_from(right, rels)
        }

        // Any comparison with a NULL operand is UNKNOWN, never TRUE. It
        // rejects as soon as one side reads the relation in question -- but
        // only if the *other* side cannot itself be NULL-tolerant nonsense,
        // which for a comparison it cannot: UNKNOWN propagates.
        BoundExprKind::Binary { op, left, right } if op.is_comparison() => {
            reads(left, rels) || reads(right, rels)
        }

        // `x BETWEEN a AND b` is a pair of comparisons and behaves the same.
        BoundExprKind::Between { expr, low, high, negated: false } => {
            reads(expr, rels) || reads(low, rels) || reads(high, rels)
        }

        // `x IN (list)` is UNKNOWN when `x` is NULL. `NOT IN` is too, which is
        // the classic trap -- and it means both forms reject.
        BoundExprKind::InList { expr, .. } => reads(expr, rels),

        BoundExprKind::Like { expr, pattern, .. } => {
            reads(expr, rels) || reads(pattern, rels)
        }

        // `NOT (x IS NULL)` is `x IS NOT NULL`, which rejects. Anything else
        // under a NOT is not worth reasoning about: `NOT (b.x > 5)` is UNKNOWN
        // on a padded row and so does reject, but `NOT` of a disjunction
        // inverts the analysis, and being wrong here is expensive.
        BoundExprKind::Unary { op: UnaryOperator::Not, expr } => match &expr.kind {
            BoundExprKind::IsNull { expr, negated: false } => reads(expr, rels),
            _ => false,
        },

        // `x IS NOT NULL` rejects; `x IS NULL` is the one construct that
        // *depends* on the padding and must never trigger this rewrite.
        BoundExprKind::IsNull { expr, negated } => *negated && reads(expr, rels),

        // A literal false rejects everything, padded or not.
        BoundExprKind::Literal(ScalarValue::Boolean(false) | ScalarValue::Null) => true,

        _ => false,
    }
}

/// Whether an expression reads any column of `rels`.
fn reads(e: &BoundExpr, rels: &std::collections::BTreeSet<RelId>) -> bool {
    let mut own = std::collections::BTreeSet::new();
    e.relations(&mut own);
    own.intersection(rels).next().is_some()
}
