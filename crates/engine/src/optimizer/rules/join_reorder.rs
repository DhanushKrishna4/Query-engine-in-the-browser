//! Cost-based join reordering.
//!
//! A chain of inner joins can be evaluated in any order, and the orders differ
//! enormously in cost: joining the two tables that produce ten rows before
//! joining the one that produces a million is the difference between a query
//! that returns and one that does not. The query text says nothing useful about
//! which order that is, so the planner has to work it out.
//!
//! ## The algorithm
//!
//! DPsize over subsets, the textbook dynamic program. For each subset of the
//! relations, keep the cheapest way to join exactly those; build subsets in
//! increasing size, so every split of a subset into two halves has both halves
//! already solved. `3^n` splits in total, which is fine to ten relations and
//! hopeless past about fifteen -- hence [`MAX_DP_RELATIONS`], beyond which a
//! greedy heuristic takes over.
//!
//! Both orientations of every split are enumerated, so the DP also picks which
//! side to build: the cost model charges several times more per row to build a
//! hash table than to probe it, so the smaller input ends up on the build side
//! without that ever being a special case.
//!
//! ## What is not reordered
//!
//! Only inner and cross joins. Outer joins are not freely associative -- moving
//! one changes which rows get NULL-padded -- so they are treated as opaque
//! atoms that the reorderable joins are arranged around.
//!
//! ## Termination
//!
//! The rewrite is only accepted when the new plan is strictly cheaper than the
//! old one. Cost therefore decreases on every application, so the rule cannot
//! cycle with itself or with any other rule.

use std::collections::HashMap;

use crate::optimizer::cost::CostModel;
use crate::optimizer::stats::{self, Estimate};
use crate::optimizer::{OptimizerContext, Rule};
use crate::plan::{join_schema, BoundExpr, JoinType, LogicalPlan};

/// Past this many relations the dynamic program stops being affordable.
pub const MAX_DP_RELATIONS: usize = 10;

/// Reordering has to beat the current plan by more than this to be worth doing.
/// Without a margin, floating-point noise could make two equivalent plans
/// alternate forever.
const IMPROVEMENT_MARGIN: f64 = 0.99;

pub struct JoinReorder;

impl Rule for JoinReorder {
    fn name(&self) -> &'static str {
        "join_reorder"
    }

    /// Reordering has to start at the *top* of a join region; firing at an
    /// inner join would reorder a sub-chain and miss the rest.
    fn whole_plan(&self) -> bool {
        true
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let mut changed = false;
        let rewritten = rewrite(plan, ctx, &mut changed);
        changed.then_some(rewritten)
    }
}

fn rewrite(plan: &LogicalPlan, ctx: &mut OptimizerContext, changed: &mut bool) -> LogicalPlan {
    if is_reorderable(plan) {
        if let Some(better) = reorder(plan, ctx) {
            *changed = true;
            return better;
        }
        // Even if this region cannot be improved, regions nested inside its
        // atoms still might be.
    }
    let children: Vec<LogicalPlan> = plan
        .children()
        .into_iter()
        .map(|c| rewrite(c, ctx, changed))
        .collect();
    plan.with_children(children)
}

fn is_reorderable(plan: &LogicalPlan) -> bool {
    matches!(
        plan,
        LogicalPlan::Join {
            join_type: JoinType::Inner | JoinType::Cross,
            ..
        }
    )
}

/// Flatten a maximal region of inner/cross joins into its inputs and the
/// conditions relating them.
fn collect_region(
    plan: &LogicalPlan,
    atoms: &mut Vec<LogicalPlan>,
    conditions: &mut Vec<BoundExpr>,
) {
    match plan {
        LogicalPlan::Join { join_type: JoinType::Inner | JoinType::Cross, on, left, right, .. } => {
            collect_region(left, atoms, conditions);
            collect_region(right, atoms, conditions);
            if let Some(c) = on {
                c.clone().split_conjuncts(conditions);
            }
        }
        // Anything else -- a scan, a filter, an outer join, an aggregate -- is
        // an atom the region is built out of.
        other => atoms.push(other.clone()),
    }
}

struct Candidate {
    plan: LogicalPlan,
    estimate: Estimate,
    cost: f64,
}

fn reorder(region: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
    let mut atoms = Vec::new();
    let mut conditions = Vec::new();
    collect_region(region, &mut atoms, &mut conditions);

    let n = atoms.len();
    if n < 2 {
        return None;
    }

    let model = CostModel::default();
    let estimates = stats::estimate_all(region, ctx.catalog);
    let original_cost = model.plan(region, &estimates).0;

    // Which relations each condition touches, as a bitmask over atoms.
    let atom_rels: Vec<_> = atoms.iter().map(|a| a.relations()).collect();
    let condition_masks: Vec<u32> = conditions
        .iter()
        .map(|c| {
            let rels = c.relation_set();
            let mut mask = 0u32;
            for (i, atom) in atom_rels.iter().enumerate() {
                if rels.iter().any(|r| atom.contains(r)) {
                    mask |= 1 << i;
                }
            }
            mask
        })
        .collect();

    let leaves: Vec<Candidate> = atoms
        .iter()
        .map(|a| {
            let estimate = stats::estimate_plan(a, ctx.catalog);
            let cost = model.plan(a, &estimates).0;
            Candidate {
                plan: a.clone(),
                estimate,
                cost,
            }
        })
        .collect();

    let best = if n <= MAX_DP_RELATIONS {
        dp(&leaves, &conditions, &condition_masks, &model, ctx)
    } else {
        greedy(&leaves, &conditions, &condition_masks, &model, ctx)
    }?;

    if best.cost >= original_cost * IMPROVEMENT_MARGIN {
        return None;
    }

    // Conditions that touch only one atom were never applicable at any join --
    // predicate pushdown normally moves those below, but if one is left it goes
    // in a filter rather than being dropped.
    let full: u32 = (1 << n) - 1;
    let leftovers: Vec<BoundExpr> = conditions
        .iter()
        .zip(&condition_masks)
        .filter(|(_, m)| **m != 0 && m.count_ones() < 2 && **m & full == **m)
        .map(|(c, _)| c.clone())
        .collect();

    Some(match BoundExpr::and_all(leftovers) {
        Some(predicate) => LogicalPlan::Filter {
            rel: ctx.ids.allocate(),
            predicate,
            input: Box::new(best.plan),
        },
        None => best.plan,
    })
}

/// DPsize: cheapest plan for every subset, built up by size.
fn dp(
    leaves: &[Candidate],
    conditions: &[BoundExpr],
    condition_masks: &[u32],
    model: &CostModel,
    ctx: &mut OptimizerContext,
) -> Option<Candidate> {
    let n = leaves.len();
    let mut best: HashMap<u32, Candidate> = HashMap::with_capacity(1 << n);
    for (i, leaf) in leaves.iter().enumerate() {
        best.insert(
            1 << i,
            Candidate {
                plan: leaf.plan.clone(),
                estimate: leaf.estimate.clone(),
                cost: leaf.cost,
            },
        );
    }

    for size in 2..=n {
        for mask in 1u32..(1 << n) {
            if mask.count_ones() as usize != size {
                continue;
            }
            let mut chosen: Option<Candidate> = None;

            // Every proper non-empty submask, which enumerates both
            // orientations of every split -- and therefore both choices of
            // build side.
            let mut sub = (mask - 1) & mask;
            while sub > 0 {
                let other = mask & !sub;
                if let (Some(l), Some(r)) = (best.get(&sub), best.get(&other)) {
                    if let Some(c) = join_candidate(l, r, conditions, condition_masks, model, ctx) {
                        if chosen.as_ref().is_none_or(|b| c.cost < b.cost) {
                            chosen = Some(c);
                        }
                    }
                }
                sub = (sub - 1) & mask;
            }

            if let Some(c) = chosen {
                best.insert(mask, c);
            }
        }
    }

    best.remove(&((1u32 << n) - 1))
}

/// Repeatedly join the pair that produces the fewest rows.
///
/// Used past [`MAX_DP_RELATIONS`], where the dynamic program's `3^n` splits stop
/// being affordable. Greedy is much worse in the worst case and usually fine,
/// which is the standard trade at that size.
fn greedy(
    leaves: &[Candidate],
    conditions: &[BoundExpr],
    condition_masks: &[u32],
    model: &CostModel,
    ctx: &mut OptimizerContext,
) -> Option<Candidate> {
    let mut pool: Vec<(u32, Candidate)> = leaves
        .iter()
        .enumerate()
        .map(|(i, l)| {
            (
                1u32 << i,
                Candidate {
                    plan: l.plan.clone(),
                    estimate: l.estimate.clone(),
                    cost: l.cost,
                },
            )
        })
        .collect();

    while pool.len() > 1 {
        let mut best: Option<(usize, usize, Candidate)> = None;
        for i in 0..pool.len() {
            for j in 0..pool.len() {
                if i == j {
                    continue;
                }
                let Some(c) =
                    join_candidate(&pool[i].1, &pool[j].1, conditions, condition_masks, model, ctx)
                else {
                    continue;
                };
                if best.as_ref().is_none_or(|(_, _, b)| c.cost < b.cost) {
                    best = Some((i, j, c));
                }
            }
        }
        let (i, j, candidate) = best?;
        let mask = pool[i].0 | pool[j].0;
        let (hi, lo) = if i > j { (i, j) } else { (j, i) };
        pool.remove(hi);
        pool.remove(lo);
        pool.push((mask, candidate));
    }
    pool.pop().map(|(_, c)| c)
}

/// Build the candidate that joins `left` and `right`, with whichever conditions
/// first become applicable at that join.
fn join_candidate(
    left: &Candidate,
    right: &Candidate,
    conditions: &[BoundExpr],
    condition_masks: &[u32],
    model: &CostModel,
    ctx: &mut OptimizerContext,
) -> Option<Candidate> {
    let _ = condition_masks;
    // A condition belongs at this join if it needs both sides.
    let left_rels = left.plan.relations();
    let right_rels = right.plan.relations();
    let applicable: Vec<BoundExpr> = conditions
        .iter()
        .filter(|c| {
            let rels = c.relation_set();
            let touches_left = rels.iter().any(|r| left_rels.contains(r));
            let touches_right = rels.iter().any(|r| right_rels.contains(r));
            let covered = rels
                .iter()
                .all(|r| left_rels.contains(r) || right_rels.contains(r));
            touches_left && touches_right && covered
        })
        .cloned()
        .collect();

    let on = BoundExpr::and_all(applicable);
    let estimate = stats::estimate_join(&left.estimate, &right.estimate, on.as_ref());

    let join_type = if on.is_some() {
        JoinType::Inner
    } else {
        JoinType::Cross
    };
    let cost = left.cost
        + right.cost
        + model
            .join(on.is_some(), left.estimate.rows, right.estimate.rows, estimate.rows)
            .0;

    let plan = LogicalPlan::Join {
        rel: ctx.ids.allocate(),
        join_type,
        schema: join_schema(join_type, &left.plan, &right.plan),
        on,
        left: Box::new(left.plan.clone()),
        right: Box::new(right.plan.clone()),
    };

    Some(Candidate { plan, estimate, cost })
}
