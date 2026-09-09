//! The rule-based optimizer, and the trace that records what it did.
//!
//! ## Shape
//!
//! Rules are independent transformations of a single plan node. The driver
//! walks the tree bottom-up, applies the *first* rule that fires, records the
//! whole plan before and after, and starts again from the bottom. That is
//! deliberately not the fastest way to run a rule set -- it is the way that
//! makes every step a complete, renderable plan, which is what the UI's
//! optimizer-trace slider consumes.
//!
//! Recording is built in from the first rule rather than added later, because
//! retrofitting it means revisiting every rule already written. There is no
//! untraced path: [`Optimizer::optimize`] just discards the trace.
//!
//! ## Trace representation
//!
//! Each step holds a full clone of the plan before and after. Plans here are
//! tens of nodes, so a clone is microseconds, and it means a step is
//! self-contained: the UI can jump to any point without replaying from the
//! start, and there is no patch format to keep in sync with the plan shape. The
//! alternative -- recording diffs -- buys memory this project does not need and
//! costs a second representation to maintain.
//!
//! ## Termination
//!
//! Rules run to fixpoint, which is only safe if no pair of rules can undo each
//! other. `MAX_APPLICATIONS` is a backstop, not a design: hitting it is a bug in
//! a rule, and the trace shows exactly which two rules are fighting.

pub mod cost;
pub mod rules;
pub mod stats;

#[cfg(test)]
mod tests;

use crate::plan::{LogicalPlan, RelId};

/// What a rule needs beyond the node it is rewriting: somewhere to get fresh
/// relation ids, and the statistics that make cost-based decisions possible.
pub struct OptimizerContext<'a> {
    pub catalog: &'a crate::catalog::Catalog,
    pub ids: RelIdAllocator,
    /// Set by a rule that found something it cannot handle and that nothing
    /// else will. Stops the loop and is reported to the user, which is how a
    /// pass that *must* succeed reports that it did not.
    pub error: Option<crate::error::Diagnostic>,
}

/// Hands out relation ids for nodes a rule creates, starting past everything
/// already in the plan so they never collide.
pub struct RelIdAllocator {
    next: u32,
}

impl RelIdAllocator {
    pub fn starting_after(plan: &LogicalPlan) -> RelIdAllocator {
        RelIdAllocator {
            next: crate::plan::max_rel_id(plan) + 1,
        }
    }

    pub fn allocate(&mut self) -> RelId {
        self.next += 1;
        RelId(self.next - 1)
    }
}

/// Cap on total rule applications for one query. Reaching it means two rules
/// are undoing each other; the trace will show the cycle.
const MAX_APPLICATIONS: usize = 500;

/// One rewrite, recorded.
#[derive(Debug, Clone)]
pub struct RuleApplication {
    pub rule: &'static str,
    /// The node the rule was applied at, so the UI can highlight the changed
    /// subtree rather than diffing two trees.
    pub target: RelId,
    pub before: LogicalPlan,
    pub after: LogicalPlan,
}

#[derive(Debug, Clone)]
pub struct OptimizerTrace {
    pub initial: LogicalPlan,
    pub steps: Vec<RuleApplication>,
    pub final_plan: LogicalPlan,
    /// True if the run stopped at `MAX_APPLICATIONS` instead of reaching a
    /// fixpoint.
    pub truncated: bool,
    /// Set when a mandatory rewrite could not be performed.
    pub error: Option<crate::error::Diagnostic>,
}

impl OptimizerTrace {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Human-readable replay, one step per rule application.
    pub fn render(&self, typed: bool) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "initial plan:");
        out.push_str(&indent(&crate::plan::explain(&self.initial, typed)));

        for (i, step) in self.steps.iter().enumerate() {
            let _ = writeln!(
                out,
                "\nstep {} -- {} at {}:",
                i + 1,
                step.rule,
                step.target
            );
            out.push_str(&indent(&crate::plan::explain(&step.after, typed)));
        }

        if self.steps.is_empty() {
            let _ = writeln!(out, "\n(no rule fired)");
        }
        if self.truncated {
            let _ = writeln!(
                out,
                "\nstopped after {MAX_APPLICATIONS} applications without reaching a fixpoint"
            );
        }
        out
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("  {l}\n"))
        .collect::<String>()
}

/// A single, independently testable rewrite.
pub trait Rule {
    fn name(&self) -> &'static str;

    /// Rewrite this node, or return `None` if the rule does not apply.
    ///
    /// Children have already been visited, so a rule sees them in their final
    /// form. Returning `Some` must be a real change -- returning an identical
    /// plan is what makes the fixpoint loop spin.
    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan>;

    /// Whether this rule must see the entire plan.
    ///
    /// Projection pushdown does: it prunes a scan's columns based on what every
    /// ancestor needs, and a subtree cannot know that. Such a rule is only
    /// offered the root, never an interior node -- getting that wrong would
    /// prune columns an ancestor was about to read.
    fn whole_plan(&self) -> bool {
        false
    }
}

pub struct Optimizer {
    rules: Vec<Box<dyn Rule>>,
}

impl Default for Optimizer {
    fn default() -> Optimizer {
        Optimizer::new()
    }
}

impl Optimizer {
    /// The default rule set, in the order they are tried at each node.
    ///
    /// The first two are *mandatory*: nothing downstream can execute a subquery
    /// expression, so one of them has to remove it. The rest are optimizations
    /// and can be switched off.
    ///
    /// Beyond that, order only decides which trace step comes first, since
    /// everything runs to fixpoint -- but folding constants before pushing
    /// predicates produces a far more readable trace, because a predicate has
    /// already been simplified by the time it moves.
    pub fn new() -> Optimizer {
        let mut rules = Optimizer::mandatory_rules();
        rules.push(Box::new(rules::ConstantFolding));
        rules.push(Box::new(rules::PredicateSimplification));
        rules.push(Box::new(rules::OuterToInner));
        rules.push(Box::new(rules::PredicatePushdown));
        rules.push(Box::new(rules::LimitPushdown));
        rules.push(Box::new(rules::JoinReorder));
        rules.push(Box::new(rules::ProjectionPushdown));
        Optimizer { rules }
    }

    /// Only the rewrites a query cannot run without.
    ///
    /// A subquery has no execution strategy of its own: either it is
    /// uncorrelated and can be evaluated once to a value, or it is a correlated
    /// `EXISTS`/`IN` in a WHERE clause and becomes a semi- or anti-join.
    /// Something has to do one of those, so these two rules always run --
    /// including when the optimizer is otherwise disabled, which is what keeps
    /// "run it unoptimized and compare" a usable test.
    pub fn mandatory() -> Optimizer {
        Optimizer {
            rules: Optimizer::mandatory_rules(),
        }
    }

    fn mandatory_rules() -> Vec<Box<dyn Rule>> {
        vec![
            // Decorrelation first: a WHERE-clause IN or EXISTS becomes a join
            // whether or not it is correlated, which beats materializing a
            // potentially enormous set of values.
            Box::new(rules::Decorrelate),
            Box::new(rules::EvaluateSubqueries),
        ]
    }

    /// An optimizer with a specific rule set. Used by the rule tests, which
    /// need to see one rule's effect without the others tidying up after it.
    pub fn with_rules(rules: Vec<Box<dyn Rule>>) -> Optimizer {
        Optimizer { rules }
    }

    pub fn optimize(
        &self,
        plan: LogicalPlan,
        catalog: &crate::catalog::Catalog,
    ) -> LogicalPlan {
        self.optimize_traced(plan, catalog).0
    }

    pub fn optimize_traced(
        &self,
        plan: LogicalPlan,
        catalog: &crate::catalog::Catalog,
    ) -> (LogicalPlan, OptimizerTrace) {
        let initial = plan.clone();
        let mut ctx = OptimizerContext {
            catalog,
            ids: RelIdAllocator::starting_after(&plan),
            error: None,
        };
        let mut current = plan;
        let mut steps = Vec::new();
        let mut truncated = false;

        while let Some((rule, target, next)) = self.apply_once(&current, &mut ctx, true) {
            steps.push(RuleApplication {
                rule,
                target,
                before: current,
                after: next.clone(),
            });
            current = next;
            if ctx.error.is_some() || steps.len() >= MAX_APPLICATIONS {
                truncated = steps.len() >= MAX_APPLICATIONS;
                break;
            }
        }

        let trace = OptimizerTrace {
            initial,
            steps,
            final_plan: current.clone(),
            truncated,
            error: ctx.error,
        };
        (current, trace)
    }

    /// Find the first applicable rewrite anywhere in the tree, bottom-up, and
    /// return the whole plan with it applied.
    ///
    /// Returning the whole plan rather than the rewritten subtree is what makes
    /// each trace step independently renderable.
    fn apply_once(
        &self,
        plan: &LogicalPlan,
        ctx: &mut OptimizerContext,
        is_root: bool,
    ) -> Option<(&'static str, RelId, LogicalPlan)> {
        let children = plan.children();
        for (i, child) in children.iter().enumerate() {
            if let Some((rule, target, new_child)) = self.apply_once(child, ctx, false) {
                let mut rebuilt: Vec<LogicalPlan> =
                    children.iter().map(|c| (*c).clone()).collect();
                rebuilt[i] = new_child;
                return Some((rule, target, plan.with_children(rebuilt)));
            }
        }
        for rule in &self.rules {
            if rule.whole_plan() && !is_root {
                continue;
            }
            if let Some(new) = rule.apply(plan, ctx) {
                return Some((rule.name(), plan.rel(), new));
            }
            if ctx.error.is_some() {
                return None;
            }
        }
        None
    }
}
