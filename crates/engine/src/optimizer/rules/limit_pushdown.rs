//! Limit pushdown.
//!
//! Moves a `LIMIT` below the projections that sit above it. That sounds like it
//! saves a little expression evaluation, and it does, but the reason it exists
//! is what it *enables*: the physical planner turns a limit directly above a
//! sort into a bounded top-N heap, and anything in between blocks that.
//!
//! `SELECT name FROM people ORDER BY age LIMIT 10` is exactly that case. The
//! sort key is not selected, so binding adds it as a hidden column and trims it
//! off again afterwards -- leaving `Limit(Project(Sort(...)))`, where the
//! projection hides the sort from the limit. Pushing the limit under it gives
//! `Project(Limit(Sort(...)))`, and a ten-row heap replaces ordering a million
//! rows.
//!
//! Only projections are pushed through. A projection emits exactly one row per
//! input row in the same order, so taking the first `n` before or after it is
//! the same thing. Nothing else on the way down has that property: a filter
//! removes rows, a join and an aggregate change how many there are, and a
//! window function needs its whole partition.
//!
//! Pushing a limit into a *scan* needs no rule: `LimitExec` stops pulling once
//! it has enough, and the scan simply stops being asked for batches.

use crate::optimizer::{OptimizerContext, Rule};
use crate::plan::LogicalPlan;

pub struct LimitPushdown;

impl Rule for LimitPushdown {
    fn name(&self) -> &'static str {
        "limit_pushdown"
    }

    fn apply(&self, plan: &LogicalPlan, _ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Limit { rel, skip, fetch, input } = plan else {
            return None;
        };
        let LogicalPlan::Project { rel: project_rel, exprs, schema, input: inner } = &**input
        else {
            return None;
        };

        Some(LogicalPlan::Project {
            rel: *project_rel,
            exprs: exprs.clone(),
            schema: std::sync::Arc::clone(schema),
            input: Box::new(LogicalPlan::Limit {
                rel: *rel,
                skip: *skip,
                fetch: *fetch,
                input: inner.clone(),
            }),
        })
    }
}
