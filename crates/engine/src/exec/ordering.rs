//! Which orderings a subtree's output already satisfies.
//!
//! This is the plan property that makes the last two operator choices possible.
//! A merge join needs both inputs sorted on the join keys; a streaming
//! aggregate needs its input sorted on the group keys. Neither can be *chosen*
//! without something that can answer "is this already sorted, and by what?" --
//! which is why both were missing until this existed.
//!
//! ## Conservative by construction
//!
//! Every function here errs toward reporting *no* ordering. Claiming an
//! ordering that does not hold is not a slow plan, it is a wrong answer: a
//! merge join over unsorted input silently drops rows, because it advances one
//! side past keys it will never look at again. So an ordering is reported only
//! when it can be proved, and "I am not sure" is spelled the same as "not
//! sorted".
//!
//! That is why only bare column references count as sort keys. `ORDER BY a + b`
//! genuinely orders the output, but recognising that `a + b` in the sort and
//! `a + b` in a join condition are the same expression means structural
//! expression equality, and `BoundExpr` deliberately does not implement it --
//! two expressions with the same shape may still read different relations.
//! Columns carry their relation in the reference, so they can be compared
//! exactly.

use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan, RelId};

/// One key of an ordering a plan's output is known to satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub rel: RelId,
    pub index: usize,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// The ordering a plan's output already has, outermost key first.
///
/// Empty means "nothing is known", which is the answer for every scan: this
/// engine never records that a table arrived sorted. The only source of
/// sortedness in a plan today is an explicit `Sort`, so an ordering here always
/// traces back to an `ORDER BY` somewhere below.
pub fn orderings(plan: &LogicalPlan) -> Vec<OrderKey> {
    match plan {
        LogicalPlan::Sort { keys, .. } => {
            // A prefix of provable keys is still an ordering; stop at the first
            // key that is not a bare column rather than discarding the rest.
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                let Some((rel, index)) = column_of(&key.expr) else {
                    break;
                };
                out.push(OrderKey {
                    rel,
                    index,
                    ascending: key.ascending,
                    nulls_first: key.nulls_first,
                });
            }
            out
        }

        // Both emit a subset of their input's rows in the order they arrived,
        // so whatever ordering held below still holds.
        LogicalPlan::Filter { input, .. } | LogicalPlan::Limit { input, .. } => orderings(input),

        // A projection emits one row per input row in the same order, but
        // renumbers the columns. An ordering survives only for the keys that
        // are still projected, and only as far as the first one that is not --
        // sorted by (a, b) with `b` dropped leaves output sorted by `a` alone.
        LogicalPlan::Project { exprs, rel, input, .. } => {
            let below = orderings(input);
            let mut out = Vec::with_capacity(below.len());
            for key in below {
                let Some(index) = exprs.iter().position(|e| {
                    column_of(e) == Some((key.rel, key.index))
                }) else {
                    break;
                };
                out.push(OrderKey {
                    rel: *rel,
                    index,
                    ..key
                });
            }
            out
        }

        // An alias renames the relation. Its output columns are the child's,
        // in the child's order, so a key on the i-th column below becomes a key
        // on the i-th column of the alias.
        //
        // The relation the key names is *not* checked against the child's own
        // `rel`: a Sort does not renumber anything, so its keys address the
        // relation beneath it rather than itself. What has to hold is that the
        // position still exists, which the schema says.
        LogicalPlan::SubqueryAlias { rel, schema, input, .. } => orderings(input)
            .into_iter()
            .take_while(|key| key.index < schema.len())
            .map(|key| OrderKey { rel: *rel, ..key })
            .collect(),

        // Everything else either reorders rows (Sort is handled above,
        // Aggregate and Distinct group them, a hash join interleaves them) or
        // has no rows of its own. None of them can promise anything.
        _ => Vec::new(),
    }
}

/// Whether `have` orders the output by exactly `want`, in that order.
///
/// A prefix match is enough: sorted by `(a, b, c)` satisfies a requirement for
/// `(a, b)`, because rows equal on `a` and `b` are still contiguous. The
/// reverse is not true, and neither is a permutation -- sorted by `(a, b)` does
/// not group rows by `b`.
pub fn satisfies(have: &[OrderKey], want: &[(RelId, usize)]) -> bool {
    want.len() <= have.len()
        && want
            .iter()
            .zip(have)
            .all(|((rel, index), key)| key.rel == *rel && key.index == *index)
}

/// Whether two orderings agree on direction and NULL placement for their first
/// `n` keys.
///
/// A merge join walks both sides with one comparison, so they have to be sorted
/// the same way. Ascending on one side and descending on the other is not a
/// merge join, it is two sorted inputs that happen to share a key.
pub fn directions_match(left: &[OrderKey], right: &[OrderKey], n: usize) -> bool {
    left.len() >= n
        && right.len() >= n
        && (0..n).all(|i| {
            left[i].ascending == right[i].ascending && left[i].nulls_first == right[i].nulls_first
        })
}

/// The relation and column a bare column reference names, or `None`.
pub fn column_of(expr: &BoundExpr) -> Option<(RelId, usize)> {
    match &expr.kind {
        BoundExprKind::Column { rel, index, .. } => Some((*rel, *index)),
        _ => None,
    }
}
