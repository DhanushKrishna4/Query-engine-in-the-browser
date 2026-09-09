//! The cost model.
//!
//! Every constant here was picked by hand and tuned until plans looked right on
//! this engine's benchmarks. They are not measured, they are not portable, and
//! they are not claimed to be either -- what a cost model needs is for the
//! *ratios* between operations to be roughly true, because it only ever compares
//! two plans against each other. Absolute units are arbitrary; treat one unit as
//! "the cost of touching one row in a scan".
//!
//! What matters and is roughly right:
//!
//! * Building a hash table costs several times more per row than probing it,
//!   which is why the smaller input should be the build side.
//! * A nested loop costs a comparison per *pair*, so it grows with the product
//!   while a hash join grows with the sum. That single ratio is what makes the
//!   planner prefer hashing whenever there is a key to hash on.
//! * Producing an output row costs something, so a join that explodes is
//!   penalised even when both its inputs are cheap.

use crate::optimizer::stats;
use crate::plan::{JoinType, LogicalPlan, SetOperator};
use std::collections::HashMap;

use crate::plan::RelId;

#[derive(Debug, Clone)]
pub struct CostModel {
    /// Reading one row from a materialized column.
    pub scan_row: f64,
    /// Evaluating a predicate or projection over one row.
    pub cpu_row: f64,
    /// Inserting one row into a hash table: hashing, allocating, chaining.
    pub hash_build_row: f64,
    /// Probing one row against a hash table.
    pub hash_probe_row: f64,
    /// One candidate pair in a nested loop.
    ///
    /// Higher than a hash probe, which looks wrong until you look at the
    /// operator: this engine's nested loop materializes every candidate pair
    /// into a batch and evaluates the condition over it, rather than running a
    /// tight comparison loop. The benchmark measures a 1M x 5 nested-loop join
    /// at roughly 2.6x the hash join, and this constant is what reproduces
    /// that ratio.
    pub compare_pair: f64,
    /// Materializing one output row of a join.
    pub emit_row: f64,
    /// Updating one group's accumulators.
    pub aggregate_row: f64,
}

impl Default for CostModel {
    fn default() -> CostModel {
        CostModel {
            scan_row: 1.0,
            cpu_row: 0.5,
            // Building is dominated by allocation and cache misses; probing is
            // one hash and one lookup. Roughly 4:1 on this engine.
            hash_build_row: 4.0,
            hash_probe_row: 1.0,
            compare_pair: 1.0,
            emit_row: 2.0,
            aggregate_row: 3.0,
        }
    }
}

/// Estimated total cost of a plan, in arbitrary units.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Cost(pub f64);

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, other: Cost) -> Cost {
        Cost(self.0 + other.0)
    }
}

impl CostModel {
    /// Cost of a hash join, given the two inputs' row counts and the output's.
    ///
    /// `build` is the right input, which is the one this engine materializes.
    pub fn hash_join(&self, probe: f64, build: f64, output: f64) -> Cost {
        Cost(build * self.hash_build_row + probe * self.hash_probe_row + output * self.emit_row)
    }

    pub fn nested_loop_join(&self, probe: f64, build: f64, output: f64) -> Cost {
        Cost(build * self.scan_row + probe * build * self.compare_pair + output * self.emit_row)
    }

    /// Whichever join implementation the physical planner would choose.
    pub fn join(&self, has_equi_key: bool, probe: f64, build: f64, output: f64) -> Cost {
        if has_equi_key {
            self.hash_join(probe, build, output)
        } else {
            self.nested_loop_join(probe, build, output)
        }
    }

    pub fn scan(&self, rows: f64, columns: usize) -> Cost {
        // A columnar scan pays per column it actually reads, which is what makes
        // projection pushdown worth costing.
        Cost(rows * self.scan_row * (columns as f64).max(1.0).sqrt())
    }

    pub fn filter(&self, rows: f64) -> Cost {
        Cost(rows * self.cpu_row)
    }

    pub fn project(&self, rows: f64) -> Cost {
        Cost(rows * self.cpu_row)
    }

    pub fn aggregate(&self, input_rows: f64, groups: f64) -> Cost {
        Cost(input_rows * self.aggregate_row + groups * self.emit_row)
    }

    /// Cost a whole plan, using estimated cardinalities.
    pub fn plan(&self, plan: &LogicalPlan, estimates: &HashMap<RelId, f64>) -> Cost {
        let rows = |p: &LogicalPlan| estimates.get(&p.rel()).copied().unwrap_or(0.0);
        let children: f64 = plan
            .children()
            .iter()
            .map(|c| self.plan(c, estimates).0)
            .sum();

        let own = match plan {
            LogicalPlan::OneRow { .. } => 0.0,
            LogicalPlan::Scan { schema, .. } => self.scan(rows(plan), schema.len()).0,
            LogicalPlan::Filter { input, .. } => self.filter(rows(input)).0,
            LogicalPlan::Project { .. } => self.project(rows(plan)).0,
            LogicalPlan::Limit { .. } | LogicalPlan::SubqueryAlias { .. } => 0.0,
            LogicalPlan::Join { join_type, on, left, right, .. } => {
                let has_key = *join_type != JoinType::Cross && on.is_some();
                self.join(has_key, rows(left), rows(right), rows(plan)).0
            }
            LogicalPlan::Aggregate { input, .. } => {
                self.aggregate(rows(input), rows(plan)).0
            }
            // Sorted once, then one pass per function.
            LogicalPlan::Window { functions, input, .. } => {
                let n = rows(input).max(1.0);
                n * n.log2().max(1.0) * self.cpu_row + n * functions.len() as f64 * self.cpu_row
            }
            // n log n comparisons, each costing a CPU tick.
            LogicalPlan::Sort { input, .. } => {
                let n = rows(input).max(1.0);
                n * n.log2().max(1.0) * self.cpu_row
            }
            // One hash and one probe per row, like a hash-table build.
            LogicalPlan::Distinct { input, .. } => rows(input) * self.hash_build_row,
            LogicalPlan::SetOp { op, all, left, right, .. } => {
                let inputs = rows(left) + rows(right);
                if *op == SetOperator::Union && *all {
                    // Nothing to identify, so nothing to hash.
                    inputs * self.cpu_row
                } else {
                    inputs * self.hash_build_row
                }
            }
        };
        Cost(children + own)
    }
}

/// Cost a plan against a catalog, estimating cardinalities as it goes.
pub fn cost_of(plan: &LogicalPlan, catalog: &crate::catalog::Catalog) -> Cost {
    let estimates = stats::estimate_all(plan, catalog);
    CostModel::default().plan(plan, &estimates)
}
