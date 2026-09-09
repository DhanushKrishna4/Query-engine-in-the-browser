//! Common subexpression elimination.
//!
//! `SELECT a + b, (a + b) * 2 FROM t` computes `a + b` twice for every row.
//! This rule computes it once, in a projection below, and has the one above
//! read the result:
//!
//! ```text
//!   Project [(a + b), (a + b) * 2]          Project [#1.0, #1.0 * 2]
//!     -> Scan t                     ->        -> Project [a, b, (a + b)]      #1
//!                                               -> Scan t
//! ```
//!
//! In a vectorized engine the saving is per *batch*, not per row: the
//! subexpression's kernel runs once over 2048 values instead of twice.
//!
//! ## What it must not hoist: anything evaluated lazily
//!
//! Hoisting does not change *whether* an expression raises. The inserted
//! projection sits directly below the original one and sees exactly the same
//! rows in the same order, so `x / y` divides by zero on the same row either
//! way. Refusing to hoist anything that `can_raise` would be the obvious
//! safeguard, and it is the wrong one -- it excludes all arithmetic, which is
//! the case this rule exists for.
//!
//! The real hazard is *lazy* positions, where the original evaluates a
//! subexpression over some rows and the hoisted version evaluates it over all
//! of them:
//!
//!   * a `CASE` branch runs only for the rows that reach it;
//!   * the right operand of `AND` and `OR` is evaluated only over the rows the
//!     left one left undecided.
//!
//! `CASE WHEN y <> 0 THEN x / y ELSE 0 END` guards the division, and lifting
//! `x / y` out throws the guard away. So candidates are never *counted* in
//! those positions. They may still be rewritten there: if the same expression
//! also occurs eagerly, it is already being evaluated for every row and the
//! guard was moot before this rule touched anything.
//!
//! ## Why a projection rather than a shared node
//!
//! The plan is a tree, so "compute once and use twice" has to be expressed as a
//! node that produces a column two expressions can read. That is exactly what a
//! projection is. The alternative -- a DAG of expressions with reference
//! counts -- is what a compiler would do, and it would mean the whole plan
//! stops being a tree.

use std::collections::HashMap;
use std::sync::Arc;

use crate::optimizer::{OptimizerContext, Rule};
use crate::parser::ast::BinaryOperator;
use crate::plan::{BoundExpr, BoundExprKind, LogicalPlan, RelId};
use crate::storage::{Field, Schema};

pub struct CommonSubexpression;

/// Below this many nodes an expression is not worth a column of its own: the
/// projection that carries it costs a buffer and a copy, and `a + 1` evaluated
/// twice is cheaper than materializing it once.
const MIN_SIZE: usize = 3;

impl Rule for CommonSubexpression {
    fn name(&self) -> &'static str {
        "common_subexpression"
    }

    fn apply(&self, plan: &LogicalPlan, ctx: &mut OptimizerContext) -> Option<LogicalPlan> {
        let LogicalPlan::Project { rel, exprs, schema, input } = plan else {
            return None;
        };
        if exprs.len() < 2 {
            return None;
        }

        // Count every hoistable subexpression across the whole projection list.
        let mut counts: HashMap<String, (usize, BoundExpr)> = HashMap::new();
        for e in exprs {
            count_candidates(e, &mut counts);
        }
        let mut repeated: Vec<(String, BoundExpr)> = counts
            .into_iter()
            .filter(|(_, (n, _))| *n >= 2)
            .map(|(k, (_, e))| (k, e))
            .collect();
        if repeated.is_empty() {
            return None;
        }
        // Deterministic order, so the plan does not depend on hash iteration.
        repeated.sort_by(|a, b| a.0.cmp(&b.0));

        // Nesting needs no special case. `rewrite` replaces the outermost
        // match first, so a repeated subexpression inside another repeated one
        // simply never appears above -- it is computed inline within the
        // column that contains it. The only cost is a hoisted column nothing
        // ends up reading, and those are pruned below rather than predicted.

        // The child projects every base column the outer expressions read,
        // followed by one column per hoisted subexpression.
        let mut base: Vec<(RelId, usize)> = Vec::new();
        for e in exprs {
            collect_columns(e, &mut base);
        }
        base.sort();
        base.dedup();

        let child_rel = ctx.ids.allocate();
        let mut slots: Vec<Slot> = Vec::new();
        let mut column_slot: HashMap<(RelId, usize), usize> = HashMap::new();
        for (rel_id, index) in &base {
            let Some(found) = find_column(exprs, *rel_id, *index) else {
                continue;
            };
            column_slot.insert((*rel_id, *index), slots.len());
            slots.push(Slot {
                name: column_name(&found),
                expr: found,
            });
        }
        let mut hoisted_slot: HashMap<String, usize> = HashMap::new();
        for (key, expr) in &repeated {
            hoisted_slot.insert(key.clone(), slots.len());
            slots.push(Slot {
                // Named in `project_over`, once compaction has settled which
                // position it ends up in -- the reference built by `rewrite`
                // uses the final index, and the two must agree.
                name: String::new(),
                expr: expr.clone(),
            });
        }

        // Rewrite once to find out which slots are actually read, drop the
        // rest, then rewrite again over the compacted layout.
        let provisional: Vec<BoundExpr> = exprs
            .iter()
            .map(|e| rewrite(e, child_rel, &hoisted_slot, &column_slot))
            .collect();
        let mut used = vec![false; slots.len()];
        for e in &provisional {
            let mut refs = Vec::new();
            collect_columns(e, &mut refs);
            for (r, i) in refs {
                if r == child_rel && i < used.len() {
                    used[i] = true;
                }
            }
        }
        if used.iter().all(|u| *u) {
            // Nothing to compact; the provisional rewrite is final.
            return Some(project_over(
                *rel, provisional, schema, child_rel, &slots, input,
            ));
        }

        let mut remap = vec![usize::MAX; slots.len()];
        let mut kept: Vec<Slot> = Vec::new();
        for (i, slot) in slots.into_iter().enumerate() {
            if used[i] {
                remap[i] = kept.len();
                kept.push(slot);
            }
        }
        let column_slot: HashMap<(RelId, usize), usize> = column_slot
            .into_iter()
            .filter(|(_, i)| remap[*i] != usize::MAX)
            .map(|(k, i)| (k, remap[i]))
            .collect();
        let hoisted_slot: HashMap<String, usize> = hoisted_slot
            .into_iter()
            .filter(|(_, i)| remap[*i] != usize::MAX)
            .map(|(k, i)| (k, remap[i]))
            .collect();
        let rewritten: Vec<BoundExpr> = exprs
            .iter()
            .map(|e| rewrite(e, child_rel, &hoisted_slot, &column_slot))
            .collect();

        Some(project_over(*rel, rewritten, schema, child_rel, &kept, input))
    }
}

/// One output column of the projection this rule inserts.
struct Slot {
    name: String,
    expr: BoundExpr,
}

fn project_over(
    rel: RelId,
    exprs: Vec<BoundExpr>,
    schema: &Arc<Schema>,
    child_rel: RelId,
    slots: &[Slot],
    input: &LogicalPlan,
) -> LogicalPlan {
    LogicalPlan::Project {
        rel,
        exprs,
        schema: Arc::clone(schema),
        input: Box::new(LogicalPlan::Project {
            rel: child_rel,
            exprs: slots.iter().map(|s| s.expr.clone()).collect(),
            schema: Arc::new(Schema::new(
                slots
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let name = if s.name.is_empty() {
                            format!("cse_{i}")
                        } else {
                            s.name.clone()
                        };
                        Field::new(name, s.expr.data_type, s.expr.nullable)
                    })
                    .collect(),
            )),
            input: Box::new(input.clone()),
        }),
    }
}

/// Count each hoistable subexpression. A `CASE` is walked into for *counting*
/// nothing -- its branches are lazily evaluated and must stay where they are.
fn count_candidates(e: &BoundExpr, out: &mut HashMap<String, (usize, BoundExpr)>) {
    if hoistable(e) {
        let entry = out.entry(structural_key(e)).or_insert((0, e.clone()));
        entry.0 += 1;
    }
    match &e.kind {
        // Every branch of a CASE is lazy. The whole CASE may still be hoisted
        // by the check above; nothing inside it may.
        BoundExprKind::Case { .. } => {}
        // `AND` and `OR` evaluate their right operand only over the rows the
        // left one left undecided, so that side is lazy too.
        BoundExprKind::Binary {
            op: BinaryOperator::And | BinaryOperator::Or,
            left,
            ..
        } => count_candidates(left, out),
        _ => for_each_child(e, &mut |c| count_candidates(c, out)),
    }
}

/// Whether an expression is worth its own column.
fn hoistable(e: &BoundExpr) -> bool {
    match &e.kind {
        // A column or a literal is already as cheap as a column reference.
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => false,
        // A subquery is not an expression that can be shared by copying it into
        // a projection; decorrelation owns those.
        BoundExprKind::Subquery { .. } => false,
        BoundExprKind::Case { .. } => false,
        _ => size(e) >= MIN_SIZE,
    }
}

fn size(e: &BoundExpr) -> usize {
    let mut n = 1;
    for_each_child(e, &mut |c| n += size(c));
    n
}

/// A structural identity for an expression: same shape, same relations, same
/// literals. `BoundExpr` has no `PartialEq`, and two expressions that render
/// the same are not necessarily the same -- `a.id` and `b.id` both print as
/// `id` -- so the relation is part of the key.
fn structural_key(e: &BoundExpr) -> String {
    let mut s = String::new();
    write_key(e, &mut s);
    s
}

fn write_key(e: &BoundExpr, out: &mut String) {
    use std::fmt::Write;
    match &e.kind {
        BoundExprKind::Column { rel, index, .. } => {
            let _ = write!(out, "c{}.{}", rel.0, index);
        }
        BoundExprKind::Literal(v) => {
            let _ = write!(out, "l({v}:{:?})", v.data_type());
        }
        BoundExprKind::Binary { op, .. } => {
            let _ = write!(out, "b({op:?}");
        }
        BoundExprKind::Unary { op, .. } => {
            let _ = write!(out, "u({op:?}");
        }
        BoundExprKind::Cast { implicit, .. } => {
            let _ = write!(out, "cast({:?},{implicit}", e.data_type);
        }
        BoundExprKind::IsNull { negated, .. } => {
            let _ = write!(out, "isnull({negated}");
        }
        BoundExprKind::Between { negated, .. } => {
            let _ = write!(out, "between({negated}");
        }
        BoundExprKind::InList { negated, .. } => {
            let _ = write!(out, "in({negated}");
        }
        BoundExprKind::Like { negated, case_insensitive, .. } => {
            let _ = write!(out, "like({negated},{case_insensitive}");
        }
        BoundExprKind::Case { .. } => out.push_str("case("),
        BoundExprKind::Subquery { .. } => {
            // Never hoisted, but it may sit inside something that is counted,
            // so it needs a key that is unique to it.
            let _ = write!(out, "sub({})", e.id.0);
        }
    }
    if matches!(
        e.kind,
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) | BoundExprKind::Subquery { .. }
    ) {
        return;
    }
    for_each_child(e, &mut |c| {
        out.push(',');
        write_key(c, out);
    });
    out.push(')');
}

fn for_each_child(e: &BoundExpr, f: &mut impl FnMut(&BoundExpr)) {
    match &e.kind {
        BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => {}
        BoundExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        BoundExprKind::Unary { expr, .. }
        | BoundExprKind::Cast { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => f(expr),
        BoundExprKind::Between { expr, low, high, .. } => {
            f(expr);
            f(low);
            f(high);
        }
        BoundExprKind::InList { expr, list, .. } => {
            f(expr);
            for i in list {
                f(i);
            }
        }
        BoundExprKind::Like { expr, pattern, escape, .. } => {
            f(expr);
            f(pattern);
            if let Some(esc) = escape {
                f(esc);
            }
        }
        BoundExprKind::Case { when_then, else_expr } => {
            for (w, t) in when_then {
                f(w);
                f(t);
            }
            if let Some(e) = else_expr {
                f(e);
            }
        }
        BoundExprKind::Subquery { .. } => {}
    }
}

fn collect_columns(e: &BoundExpr, out: &mut Vec<(RelId, usize)>) {
    if let BoundExprKind::Column { rel, index, .. } = &e.kind {
        out.push((*rel, *index));
    }
    for_each_child(e, &mut |c| collect_columns(c, out));
}

/// Find an existing reference to a column, to copy its name and type.
fn find_column(exprs: &[BoundExpr], rel: RelId, index: usize) -> Option<BoundExpr> {
    fn walk(e: &BoundExpr, rel: RelId, index: usize) -> Option<BoundExpr> {
        if let BoundExprKind::Column { rel: r, index: i, .. } = &e.kind {
            if *r == rel && *i == index {
                return Some(e.clone());
            }
        }
        let mut found = None;
        for_each_child(e, &mut |c| {
            if found.is_none() {
                found = walk(c, rel, index);
            }
        });
        found
    }
    exprs.iter().find_map(|e| walk(e, rel, index))
}

fn column_name(e: &BoundExpr) -> String {
    match &e.kind {
        BoundExprKind::Column { name, .. } => name.clone(),
        _ => "col".to_string(),
    }
}

/// Rewrite an expression over the child projection's output.
fn rewrite(
    e: &BoundExpr,
    child: RelId,
    hoisted: &HashMap<String, usize>,
    columns: &HashMap<(RelId, usize), usize>,
) -> BoundExpr {
    // A hoisted subexpression becomes a reference to the column holding it.
    if let Some(slot) = hoisted.get(&structural_key(e)) {
        return BoundExpr {
            id: e.id,
            kind: BoundExprKind::Column {
                rel: child,
                index: *slot,
                name: format!("cse_{slot}"),
            },
            data_type: e.data_type,
            nullable: e.nullable,
            span: e.span,
        };
    }
    if let BoundExprKind::Column { rel, index, name } = &e.kind {
        let slot = columns
            .get(&(*rel, *index))
            .copied()
            .expect("every column read above was projected below");
        return BoundExpr {
            id: e.id,
            kind: BoundExprKind::Column {
                rel: child,
                index: slot,
                name: name.clone(),
            },
            data_type: e.data_type,
            nullable: e.nullable,
            span: e.span,
        };
    }
    map_children(e, &mut |c| rewrite(c, child, hoisted, columns))
}

/// Rebuild an expression with each child transformed.
fn map_children(e: &BoundExpr, f: &mut impl FnMut(&BoundExpr) -> BoundExpr) -> BoundExpr {
    let kind = match &e.kind {
        BoundExprKind::Binary { op, left, right } => BoundExprKind::Binary {
            op: *op,
            left: Box::new(f(left)),
            right: Box::new(f(right)),
        },
        BoundExprKind::Unary { op, expr } => BoundExprKind::Unary {
            op: *op,
            expr: Box::new(f(expr)),
        },
        BoundExprKind::Cast { expr, implicit } => BoundExprKind::Cast {
            expr: Box::new(f(expr)),
            implicit: *implicit,
        },
        BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
            expr: Box::new(f(expr)),
            negated: *negated,
        },
        BoundExprKind::Between { expr, low, high, negated } => BoundExprKind::Between {
            expr: Box::new(f(expr)),
            low: Box::new(f(low)),
            high: Box::new(f(high)),
            negated: *negated,
        },
        BoundExprKind::InList { expr, list, negated } => BoundExprKind::InList {
            expr: Box::new(f(expr)),
            list: list.iter().map(&mut *f).collect(),
            negated: *negated,
        },
        BoundExprKind::Like { expr, pattern, escape, negated, case_insensitive } => {
            BoundExprKind::Like {
                expr: Box::new(f(expr)),
                pattern: Box::new(f(pattern)),
                escape: escape.as_ref().map(|x| Box::new(f(x))),
                negated: *negated,
                case_insensitive: *case_insensitive,
            }
        }
        BoundExprKind::Case { when_then, else_expr } => BoundExprKind::Case {
            when_then: when_then.iter().map(|(w, t)| (f(w), f(t))).collect(),
            else_expr: else_expr.as_ref().map(|x| Box::new(f(x))),
        },
        other => other.clone(),
    };
    BoundExpr {
        id: e.id,
        kind,
        data_type: e.data_type,
        nullable: e.nullable,
        span: e.span,
    }
}
