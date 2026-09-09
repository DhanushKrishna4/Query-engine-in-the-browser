//! The reference evaluator: one row at a time, one `ScalarValue` per value.
//!
//! Nothing here is fast and nothing here is supposed to be. It exists so that
//! the meaning of every expression is written down once in a form that can be
//! read and checked by eye, and so that [`crate::expr::vector`] has something to
//! be differentially tested against. When the two disagree, this one is right
//! until proven otherwise.

use std::cmp::Ordering;

use crate::error::{Diagnostic, Result};
use crate::expr::{from_tri, like_match, three_valued_and, three_valued_not, three_valued_or, EvalContext};
use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind};
use crate::storage::{Batch, Column, ColumnBuilder};
use crate::types::{self, ScalarValue};

/// Evaluate `expr` for a single row.
pub fn eval(expr: &BoundExpr, batch: &Batch, row: usize, ctx: &EvalContext) -> Result<ScalarValue> {
    match &expr.kind {
        BoundExprKind::Literal(v) => Ok(v.clone()),

        BoundExprKind::Column { rel, index, name } => {
            let position = ctx.position(*rel, *index).ok_or_else(|| {
                Diagnostic::exec(format!(
                    "internal: column `{name}` of relation {rel} is not present in this batch"
                ))
                .with_span(expr.span)
            })?;
            Ok(batch.value(position, row))
        }

        BoundExprKind::Cast { expr: inner, .. } => {
            let v = eval(inner, batch, row, ctx)?;
            types::cast_scalar(&v, &expr.data_type).map_err(|d| d.with_span(expr.span))
        }

        BoundExprKind::IsNull { expr: inner, negated } => {
            let v = eval(inner, batch, row, ctx)?;
            Ok(ScalarValue::Boolean(v.is_null() != *negated))
        }

        BoundExprKind::Unary { op, expr: inner } => {
            let v = eval(inner, batch, row, ctx)?;
            eval_unary(*op, v, expr)
        }

        BoundExprKind::Binary { op, left, right } => eval_binary(*op, left, right, batch, row, ctx, expr),

        BoundExprKind::Between { expr: inner, low, high, negated } => {
            let v = eval(inner, batch, row, ctx)?;
            let lo = eval(low, batch, row, ctx)?;
            let hi = eval(high, batch, row, ctx)?;
            // `x BETWEEN a AND b` is `x >= a AND x <= b`, including its NULL
            // behaviour: if any of the three is NULL the answer is UNKNOWN,
            // unless one side already settles it as FALSE.
            let ge = compare_op(BinaryOperator::GtEq, &v, &lo);
            let le = compare_op(BinaryOperator::LtEq, &v, &hi);
            let inside = three_valued_and(ge, le);
            Ok(from_tri(if *negated { three_valued_not(inside) } else { inside }))
        }

        BoundExprKind::InList { expr: inner, list, negated } => {
            // An empty set settles the answer before the value matters:
            // `x IN ()` is FALSE and `x NOT IN ()` is TRUE, even for a NULL x,
            // because there is nothing for it to fail to match. Reachable once
            // a subquery supplies the list.
            if list.is_empty() {
                return Ok(ScalarValue::Boolean(*negated));
            }
            let v = eval(inner, batch, row, ctx)?;
            if v.is_null() {
                return Ok(ScalarValue::Null);
            }
            let mut saw_null = false;
            let mut found = false;
            // Every item is evaluated even after a match. Stopping early would
            // mean `2 IN (2, 1 / 0)` succeeds here and raises in the vectorized
            // evaluator, which evaluates the list as columns -- and the two must
            // agree. SQL does not fix an evaluation order, so eager is a valid
            // choice; it just has to be the same choice on both sides.
            for item in list {
                let iv = eval(item, batch, row, ctx)?;
                if iv.is_null() {
                    saw_null = true;
                    continue;
                }
                if types::compare(&v, &iv) == Some(Ordering::Equal) {
                    found = true;
                }
            }
            // The NOT IN trap: with no match but a NULL in the list, the answer
            // is UNKNOWN rather than FALSE, so `x NOT IN (1, NULL)` returns no
            // rows at all instead of "every row where x <> 1".
            let tri = if found {
                Some(true)
            } else if saw_null {
                None
            } else {
                Some(false)
            };
            Ok(from_tri(if *negated { three_valued_not(tri) } else { tri }))
        }

        BoundExprKind::Like {
            expr: inner, pattern, escape, negated, case_insensitive,
        } => {
            let v = eval(inner, batch, row, ctx)?;
            let p = eval(pattern, batch, row, ctx)?;
            let esc = match escape {
                Some(e) => Some(eval(e, batch, row, ctx)?),
                None => None,
            };
            if v.is_null() || p.is_null() || esc.as_ref().is_some_and(|e| e.is_null()) {
                return Ok(ScalarValue::Null);
            }
            let (ScalarValue::Utf8(text), ScalarValue::Utf8(pat)) = (&v, &p) else {
                return Err(Diagnostic::exec("internal: LIKE operands must be strings")
                    .with_span(expr.span));
            };
            let escape_char = match &esc {
                Some(ScalarValue::Utf8(s)) => {
                    let mut chars = s.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => Some(c),
                        _ => {
                            return Err(Diagnostic::exec(
                                "ESCAPE must be a single-character string",
                            )
                            .with_span(expr.span))
                        }
                    }
                }
                _ => None,
            };
            let m = like_match(text, pat, *case_insensitive, escape_char);
            Ok(ScalarValue::Boolean(m != *negated))
        }

        // Subqueries are removed during planning: uncorrelated ones are
        // evaluated to a value, correlated ones become semi- or anti-joins. If
        // one survives to here, planning let something through.
        BoundExprKind::Subquery { kind, .. } => Err(Diagnostic::exec(format!(
            "internal: a {} subquery reached execution",
            kind.label()
        ))
        .with_span(expr.span)),

        BoundExprKind::Case { when_then, else_expr } => {
            for (cond, value) in when_then {
                // Only TRUE selects a branch. A NULL condition is not a match,
                // which is what makes `CASE x WHEN NULL THEN ...` fall through
                // to ELSE rather than matching NULL rows.
                if eval(cond, batch, row, ctx)?.as_bool() == Some(true) {
                    return eval(value, batch, row, ctx);
                }
            }
            match else_expr {
                Some(e) => eval(e, batch, row, ctx),
                None => Ok(ScalarValue::Null),
            }
        }
    }
}

fn eval_unary(op: UnaryOperator, v: ScalarValue, expr: &BoundExpr) -> Result<ScalarValue> {
    match op {
        UnaryOperator::Not => Ok(from_tri(three_valued_not(v.as_bool()))),
        UnaryOperator::Plus => Ok(v),
        UnaryOperator::Minus => match v {
            ScalarValue::Null => Ok(ScalarValue::Null),
            ScalarValue::Int32(i) => i
                .checked_neg()
                .map(ScalarValue::Int32)
                .ok_or_else(|| overflow(expr)),
            ScalarValue::Int64(i) => i
                .checked_neg()
                .map(ScalarValue::Int64)
                .ok_or_else(|| overflow(expr)),
            ScalarValue::Float64(f) => Ok(ScalarValue::Float64(-f)),
            other => Err(Diagnostic::exec(format!(
                "unary `-` is not defined for {}",
                other.data_type()
            ))
            .with_span(expr.span)),
        },
    }
}

fn eval_binary(
    op: BinaryOperator,
    left: &BoundExpr,
    right: &BoundExpr,
    batch: &Batch,
    row: usize,
    ctx: &EvalContext,
    expr: &BoundExpr,
) -> Result<ScalarValue> {
    // AND / OR short-circuit. This is not just an optimization: it is what
    // stops `WHERE a <> 0 AND 100 / a > 1` from raising a division error on the
    // rows the first conjunct already rejected.
    if op == BinaryOperator::And {
        let l = eval(left, batch, row, ctx)?.as_bool();
        if l == Some(false) {
            return Ok(ScalarValue::Boolean(false));
        }
        let r = eval(right, batch, row, ctx)?.as_bool();
        return Ok(from_tri(three_valued_and(l, r)));
    }
    if op == BinaryOperator::Or {
        let l = eval(left, batch, row, ctx)?.as_bool();
        if l == Some(true) {
            return Ok(ScalarValue::Boolean(true));
        }
        let r = eval(right, batch, row, ctx)?.as_bool();
        return Ok(from_tri(three_valued_or(l, r)));
    }

    let l = eval(left, batch, row, ctx)?;
    let r = eval(right, batch, row, ctx)?;

    if op.is_comparison() {
        return Ok(from_tri(compare_op(op, &l, &r)));
    }

    // Every remaining operator propagates NULL unconditionally.
    if l.is_null() || r.is_null() {
        return Ok(ScalarValue::Null);
    }

    if op == BinaryOperator::StringConcat {
        return match (&l, &r) {
            (ScalarValue::Utf8(a), ScalarValue::Utf8(b)) => {
                Ok(ScalarValue::Utf8(format!("{a}{b}")))
            }
            _ => Err(Diagnostic::exec("internal: `||` operands must be strings")
                .with_span(expr.span)),
        };
    }

    arithmetic(op, &l, &r, expr)
}

fn arithmetic(
    op: BinaryOperator,
    l: &ScalarValue,
    r: &ScalarValue,
    expr: &BoundExpr,
) -> Result<ScalarValue> {
    use BinaryOperator::*;
    match (l, r) {
        (ScalarValue::Int32(a), ScalarValue::Int32(b)) => {
            let (a, b) = (*a, *b);
            let v = match op {
                Plus => a.checked_add(b),
                Minus => a.checked_sub(b),
                Multiply => a.checked_mul(b),
                Divide => {
                    check_nonzero(b == 0, expr)?;
                    a.checked_div(b)
                }
                Modulo => {
                    check_nonzero(b == 0, expr)?;
                    a.checked_rem(b)
                }
                _ => None,
            };
            v.map(ScalarValue::Int32).ok_or_else(|| overflow(expr))
        }
        (ScalarValue::Int64(a), ScalarValue::Int64(b)) => {
            let (a, b) = (*a, *b);
            let v = match op {
                Plus => a.checked_add(b),
                Minus => a.checked_sub(b),
                Multiply => a.checked_mul(b),
                Divide => {
                    check_nonzero(b == 0, expr)?;
                    a.checked_div(b)
                }
                Modulo => {
                    check_nonzero(b == 0, expr)?;
                    a.checked_rem(b)
                }
                _ => None,
            };
            v.map(ScalarValue::Int64).ok_or_else(|| overflow(expr))
        }
        (ScalarValue::Float64(a), ScalarValue::Float64(b)) => {
            let (a, b) = (*a, *b);
            let v = match op {
                Plus => a + b,
                Minus => a - b,
                Multiply => a * b,
                Divide => {
                    // SQL raises on division by zero rather than producing an
                    // infinity, even in floating point.
                    check_nonzero(b == 0.0, expr)?;
                    a / b
                }
                Modulo => {
                    check_nonzero(b == 0.0, expr)?;
                    a % b
                }
                _ => {
                    return Err(Diagnostic::exec(format!(
                        "internal: `{}` is not an arithmetic operator",
                        op.as_str()
                    ))
                    .with_span(expr.span))
                }
            };
            Ok(ScalarValue::Float64(v))
        }
        _ => Err(Diagnostic::exec(format!(
            "operator `{}` is not defined for {} and {}",
            op.as_str(),
            l.data_type(),
            r.data_type()
        ))
        .with_span(expr.span)),
    }
}

fn check_nonzero(is_zero: bool, expr: &BoundExpr) -> Result<()> {
    if is_zero {
        Err(Diagnostic::exec("division by zero").with_span(expr.span))
    } else {
        Ok(())
    }
}

fn overflow(expr: &BoundExpr) -> Diagnostic {
    Diagnostic::exec("arithmetic overflow").with_span(expr.span)
}

pub(crate) fn compare_op(op: BinaryOperator, l: &ScalarValue, r: &ScalarValue) -> Option<bool> {
    use BinaryOperator::*;
    // `compare` returns None when either side is NULL, and that None flows
    // straight through as UNKNOWN.
    let ord = types::compare(l, r)?;
    Some(match op {
        Eq => ord == Ordering::Equal,
        NotEq => ord != Ordering::Equal,
        Lt => ord == Ordering::Less,
        LtEq => ord != Ordering::Greater,
        Gt => ord == Ordering::Greater,
        GtEq => ord != Ordering::Less,
        _ => return None,
    })
}

// -- column-level entry points ----------------------------------------------

/// Evaluate an expression over every live row of a batch, producing one dense
/// column. No fast paths: this is the reference, so every value goes through
/// `eval` exactly as written.
pub fn eval_column(expr: &BoundExpr, batch: &Batch, ctx: &EvalContext) -> Result<Column> {
    let mut builder = ColumnBuilder::new(&expr.data_type);
    for row in 0..batch.num_rows() {
        let v = eval(expr, batch, row, ctx)?;
        builder.append(&v)?;
    }
    Ok(builder.finish())
}

/// Evaluate a predicate over a batch, returning the indices of surviving rows.
///
/// Only `TRUE` survives. `FALSE` and `NULL` are both rejected, which is the
/// whole of SQL's filter semantics in one line.
pub fn eval_predicate(expr: &BoundExpr, batch: &Batch, ctx: &EvalContext) -> Result<Vec<u32>> {
    let mut keep = Vec::new();
    for row in 0..batch.num_rows() {
        if eval(expr, batch, row, ctx)?.as_bool() == Some(true) {
            keep.push(row as u32);
        }
    }
    Ok(keep)
}

