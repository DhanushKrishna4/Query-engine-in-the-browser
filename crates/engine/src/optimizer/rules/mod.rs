//! The rewrite rules.
//!
//! Each is an independent transformation of one plan node, testable on its own
//! by running an optimizer built from that rule alone.

mod common_subexpression;
mod constant_folding;
mod decorrelate;
mod join_reorder;
mod limit_pushdown;
mod outer_to_inner;
mod predicate_pushdown;
mod predicate_simplification;
mod projection_pushdown;

pub use common_subexpression::CommonSubexpression;
pub use constant_folding::ConstantFolding;
pub use decorrelate::{Decorrelate, EvaluateSubqueries};
pub use join_reorder::{JoinReorder, MAX_DP_RELATIONS};
pub use limit_pushdown::LimitPushdown;
pub use outer_to_inner::OuterToInner;
pub use predicate_pushdown::PredicatePushdown;
pub use predicate_simplification::PredicateSimplification;
pub use projection_pushdown::ProjectionPushdown;
