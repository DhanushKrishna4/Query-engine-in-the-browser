//! Projection pushdown: stop scans reading columns nothing needs.
//!
//! A columnar engine should never touch a column a query does not mention, and
//! this is the rule that makes that true. It is a whole-plan rewrite rather
//! than a local one, because whether a scan needs a column depends on what
//! every ancestor reads -- a subtree cannot answer that, which is why
//! `whole_plan` returns true and the driver only ever offers it the root.
//!
//! What it saves depends on the operator above. A scan is already zero-copy, so
//! an unread column costs nothing while it is only being carried; the saving
//! appears wherever something *materializes* -- a join gathering both sides'
//! columns into output rows, or a hash join copying its build side into memory.
//! It also becomes the difference between reading and not reading a column
//! chunk once storage is a Parquet file rather than an in-memory table.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::optimizer::{OptimizerContext, Rule};
use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan, RelId};
use crate::storage::Schema;

pub struct ProjectionPushdown;

impl Rule for ProjectionPushdown {
    fn name(&self) -> &'static str {
        "projection_pushdown"
    }

    fn whole_plan(&self) -> bool {
        true
    }

    fn apply(&self, plan: &LogicalPlan, _ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let mut required = BTreeSet::new();
        collect_required(plan, &mut required);
        let mut changed = false;
        let pruned = prune(plan, &required, &mut changed);
        changed.then_some(pruned)
    }
}

/// Every `(relation, column)` any expression in the plan reads.
fn collect_required(plan: &LogicalPlan, out: &mut BTreeSet<(RelId, usize)>) {
    match plan {
        LogicalPlan::OneRow { .. } | LogicalPlan::Scan { .. } => {}
        LogicalPlan::Filter { predicate, .. } => columns_of(predicate, out),
        LogicalPlan::Project { exprs, .. } => {
            for e in exprs {
                columns_of(e, out);
            }
        }
        LogicalPlan::Limit { .. } | LogicalPlan::SubqueryAlias { .. } => {}
        LogicalPlan::Sort { keys, .. } => {
            for k in keys {
                columns_of(&k.expr, out);
            }
        }
        // A distinct or a set operation compares whole rows, so every column of
        // its input is needed whether or not anything above reads it.
        LogicalPlan::Distinct { .. } | LogicalPlan::SetOp { .. } => {}
        LogicalPlan::Window { partition_by, order_by, functions, .. } => {
            for e in partition_by {
                columns_of(e, out);
            }
            for k in order_by {
                columns_of(&k.expr, out);
            }
            for w in functions {
                for a in &w.args {
                    columns_of(a, out);
                }
            }
        }
        LogicalPlan::Join { on, .. } => {
            if let Some(c) = on {
                columns_of(c, out);
            }
        }
        LogicalPlan::Aggregate { group_exprs, aggregates, .. } => {
            for e in group_exprs {
                columns_of(e, out);
            }
            for a in aggregates {
                if let Some(e) = &a.arg {
                    columns_of(e, out);
                }
            }
        }
    }
    for child in plan.children() {
        collect_required(child, out);
    }
}

fn columns_of(e: &BoundExpr, out: &mut BTreeSet<(RelId, usize)>) {
    if let BoundExprKind::Column { rel, index, .. } = &e.kind {
        out.insert((*rel, *index));
    }
    for child in expr_children(e) {
        columns_of(child, out);
    }
}

fn expr_children(e: &BoundExpr) -> Vec<&BoundExpr> {
    match &e.kind {
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => Vec::new(),
        BoundExprKind::Binary { left, right, .. } => vec![left, right],
        BoundExprKind::Unary { expr, .. }
        | BoundExprKind::Cast { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => vec![expr],
        BoundExprKind::Between { expr, low, high, .. } => vec![expr, low, high],
        BoundExprKind::InList { expr, list, .. } => {
            let mut v = vec![&**expr];
            v.extend(list.iter());
            v
        }
        BoundExprKind::Like { expr, pattern, escape, .. } => {
            let mut v = vec![&**expr, &**pattern];
            if let Some(x) = escape {
                v.push(x);
            }
            v
        }
        // A subquery's own columns are accounted for inside its plan; from out
        // here only the correlated references matter, and those are column
        // nodes reached through `relations`.
        BoundExprKind::Subquery { kind, .. } => match kind {
            crate::plan::SubqueryKind::In { expr, .. } => vec![&**expr],
            _ => Vec::new(),
        },
        BoundExprKind::Case { when_then, else_expr } => {
            let mut v = Vec::new();
            for (w, t) in when_then {
                v.push(w);
                v.push(t);
            }
            if let Some(e) = else_expr {
                v.push(e);
            }
            v
        }
    }
}

fn prune(
    plan: &LogicalPlan,
    required: &BTreeSet<(RelId, usize)>,
    changed: &mut bool,
) -> LogicalPlan {
    if let LogicalPlan::Scan { rel, table_name, table_schema, projection, .. } = plan {
        let wanted: Vec<usize> = required
            .iter()
            .filter(|(r, _)| r == rel)
            .map(|(_, i)| *i)
            .collect();

        // Already narrower than what is wanted, or nothing to remove.
        let current = projection
            .clone()
            .unwrap_or_else(|| (0..table_schema.len()).collect());
        if wanted.len() >= current.len() {
            return plan.clone();
        }

        *changed = true;
        let fields = wanted
            .iter()
            .map(|i| table_schema.field(*i).clone())
            .collect();
        return LogicalPlan::Scan {
            rel: *rel,
            table_name: table_name.clone(),
            table_schema: Arc::clone(table_schema),
            projection: Some(wanted),
            schema: Arc::new(Schema::new(fields)),
        };
    }

    let children: Vec<LogicalPlan> = plan
        .children()
        .into_iter()
        .map(|c| prune(c, required, changed))
        .collect();
    plan.with_children(children)
}
