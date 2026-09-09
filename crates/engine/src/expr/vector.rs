//! The vectorized evaluator: batch at a time, over whole columns.
//!
//! Three things make this fast, in decreasing order of how much they matter:
//!
//! 1. **The tree is walked once per batch, not once per row.** The scalar
//!    evaluator pays an enum dispatch and a `ScalarValue` construction for
//!    every node of every row. Here each node runs one loop over ~2048 values.
//! 2. **Names are resolved at compile time.** `compile` turns a
//!    `(RelId, index)` column reference into a physical column index once, so
//!    the hot loop does no lookups.
//! 3. **The inner loops are shaped so LLVM can vectorize them**: contiguous
//!    slices, no bounds checks, no branches, results packed 64 comparisons at a
//!    time into a `u64`. On wasm32 with `simd128` enabled (see
//!    `.cargo/config.toml`) those become v128 ops; natively they become NEON or
//!    AVX. There are no hand-written intrinsics -- see the README for what is
//!    and is not actually vectorized.
//!
//! NULLs never branch the value loop. Values are computed for every row
//! including the NULL ones (whose slots hold a harmless placeholder), and the
//! output validity is the bitwise AND of the input validity masks. That is what
//! keeps the arithmetic and comparison loops straight-line.

use std::sync::Arc;

use crate::error::{Diagnostic, Result, Span};
use crate::expr::{like_match, EvalContext};
use crate::parser::ast::{BinaryOperator, UnaryOperator};
use crate::plan::{BoundExpr, BoundExprKind};
use crate::storage::column::StringColumn;
use crate::storage::{Batch, Bitmap, Column, ColumnBuilder, ColumnData, Selection};
use crate::types::{self, DataType, ScalarValue};

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CompiledExpr {
    pub node: Node,
    pub data_type: DataType,
    pub nullable: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Node {
    /// Physical column index within the batch, resolved once at compile time.
    Column(usize),
    Literal(ScalarValue),
    Binary {
        op: BinaryOperator,
        left: Box<CompiledExpr>,
        right: Box<CompiledExpr>,
    },
    Unary {
        op: UnaryOperator,
        expr: Box<CompiledExpr>,
    },
    Cast {
        expr: Box<CompiledExpr>,
    },
    IsNull {
        expr: Box<CompiledExpr>,
        negated: bool,
    },
    Between {
        expr: Box<CompiledExpr>,
        low: Box<CompiledExpr>,
        high: Box<CompiledExpr>,
        negated: bool,
    },
    InList {
        expr: Box<CompiledExpr>,
        list: Vec<CompiledExpr>,
        negated: bool,
    },
    Like {
        expr: Box<CompiledExpr>,
        pattern: Box<CompiledExpr>,
        escape: Option<Box<CompiledExpr>>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        when_then: Vec<(CompiledExpr, CompiledExpr)>,
        else_expr: Option<Box<CompiledExpr>>,
    },
}

impl CompiledExpr {
    /// Whether evaluating this subtree could raise: division by zero, integer
    /// overflow, or a failing CAST. Conservative -- it says yes whenever it is
    /// not certain, because the answer gates a correctness decision in
    /// `eval_conjunction`, not just a performance one.
    pub fn can_raise(&self) -> bool {
        match &self.node {
            Node::Column(_) | Node::Literal(_) => false,
            Node::Cast { .. } => true,
            Node::Binary { op, left, right } => {
                op.is_arithmetic() || left.can_raise() || right.can_raise()
            }
            Node::Unary { op, expr } => {
                *op == UnaryOperator::Minus || expr.can_raise()
            }
            Node::IsNull { expr, .. } => expr.can_raise(),
            Node::Between { expr, low, high, .. } => {
                expr.can_raise() || low.can_raise() || high.can_raise()
            }
            Node::InList { expr, list, .. } => {
                expr.can_raise() || list.iter().any(|e| e.can_raise())
            }
            Node::Like { expr, pattern, escape, .. } => {
                expr.can_raise()
                    || pattern.can_raise()
                    // A malformed ESCAPE is a runtime error.
                    || escape.is_some()
            }
            Node::Case { when_then, else_expr } => {
                when_then.iter().any(|(w, t)| w.can_raise() || t.can_raise())
                    || else_expr.as_ref().is_some_and(|e| e.can_raise())
            }
        }
    }
}

/// Resolve a bound expression against the layout of the batches it will see.
pub fn compile(expr: &BoundExpr, ctx: &EvalContext) -> Result<CompiledExpr> {
    let node = match &expr.kind {
        BoundExprKind::Column { rel, index, name } => {
            let position = ctx.position(*rel, *index).ok_or_else(|| {
                Diagnostic::exec(format!(
                    "internal: column `{name}` of relation {rel} is not present in this batch"
                ))
                .with_span(expr.span)
            })?;
            Node::Column(position)
        }
        BoundExprKind::Literal(v) => Node::Literal(v.clone()),
        BoundExprKind::Binary { op, left, right } => Node::Binary {
            op: *op,
            left: Box::new(compile(left, ctx)?),
            right: Box::new(compile(right, ctx)?),
        },
        BoundExprKind::Unary { op, expr } => Node::Unary {
            op: *op,
            expr: Box::new(compile(expr, ctx)?),
        },
        BoundExprKind::Cast { expr, .. } => Node::Cast {
            expr: Box::new(compile(expr, ctx)?),
        },
        BoundExprKind::IsNull { expr, negated } => Node::IsNull {
            expr: Box::new(compile(expr, ctx)?),
            negated: *negated,
        },
        BoundExprKind::Between { expr, low, high, negated } => Node::Between {
            expr: Box::new(compile(expr, ctx)?),
            low: Box::new(compile(low, ctx)?),
            high: Box::new(compile(high, ctx)?),
            negated: *negated,
        },
        BoundExprKind::InList { expr, list, negated } => Node::InList {
            expr: Box::new(compile(expr, ctx)?),
            list: list.iter().map(|e| compile(e, ctx)).collect::<Result<_>>()?,
            negated: *negated,
        },
        BoundExprKind::Like {
            expr, pattern, escape, negated, case_insensitive,
        } => Node::Like {
            expr: Box::new(compile(expr, ctx)?),
            pattern: Box::new(compile(pattern, ctx)?),
            escape: match escape {
                Some(e) => Some(Box::new(compile(e, ctx)?)),
                None => None,
            },
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        BoundExprKind::Subquery { kind, .. } => {
            return Err(Diagnostic::exec(format!(
                "internal: a {} subquery reached execution",
                kind.label()
            ))
            .with_span(expr.span))
        }
        BoundExprKind::Case { when_then, else_expr } => Node::Case {
            when_then: when_then
                .iter()
                .map(|(w, t)| Ok((compile(w, ctx)?, compile(t, ctx)?)))
                .collect::<Result<_>>()?,
            else_expr: match else_expr {
                Some(e) => Some(Box::new(compile(e, ctx)?)),
                None => None,
            },
        },
    };
    Ok(CompiledExpr {
        node,
        data_type: expr.data_type,
        nullable: expr.nullable,
        span: expr.span,
    })
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Evaluate over every live row, producing a dense column of `batch.num_rows()`
/// values.
///
/// The result is `Arc` so that a bare column reference over an unfiltered batch
/// costs a refcount bump rather than a copy -- which is the common case for the
/// projection at the top of a plan.
pub fn eval(expr: &CompiledExpr, batch: &Batch) -> Result<Arc<Column>> {
    eval_node(expr, batch)
}

/// Evaluate a predicate and return the *logical* positions that survive.
///
/// Only `TRUE` survives: `FALSE` and `NULL` both fail. That is the whole of
/// SQL's filter semantics, and here it is one bitmap AND -- the value bits
/// masked by the validity bits.
pub fn eval_predicate(expr: &CompiledExpr, batch: &Batch) -> Result<Vec<u32>> {
    let col = eval_node(expr, batch)?;
    let ColumnData::Boolean(values) = &col.data else {
        // A NULL-typed predicate (`WHERE NULL`) selects nothing.
        if matches!(col.data, ColumnData::Null(_)) {
            return Ok(Vec::new());
        }
        return Err(Diagnostic::exec("internal: predicate did not evaluate to a boolean")
            .with_span(expr.span));
    };
    Ok(match &col.validity {
        None => values.set_indices(),
        Some(valid) => values.and(valid).set_indices(),
    })
}

// ---------------------------------------------------------------------------
// Operands
// ---------------------------------------------------------------------------

/// A column plus the selection that says which of its rows are live. Kernels
/// take these so that a leaf column never has to be materialized just to be
/// compared against something.
#[derive(Clone, Copy)]
struct Operand<'a> {
    col: &'a Column,
    sel: &'a Selection,
}

impl<'a> Operand<'a> {
    fn len(&self) -> usize {
        self.sel.len()
    }

    /// The validity of the live rows, densified. `None` means all valid.
    fn validity(&self) -> Option<Bitmap> {
        match &self.col.validity {
            None => match self.col.data {
                // An all-NULL column has no bitmap but is entirely invalid.
                ColumnData::Null(_) => Some(Bitmap::all_unset(self.len())),
                _ => None,
            },
            Some(v) => Some(match self.sel {
                Selection::Range { offset, len } => v.slice(*offset, *len),
                Selection::Indices(idx) => {
                    idx.iter().map(|i| v.get(*i as usize)).collect()
                }
            }),
        }
    }
}

/// A dense column is its own selection.
fn dense(col: &Column) -> (Selection, ()) {
    (Selection::full(col.len()), ())
}

fn combine(a: Option<Bitmap>, b: Option<Bitmap>) -> Option<Bitmap> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.and(&y)),
    }
}

// ---------------------------------------------------------------------------
// Bit packing
// ---------------------------------------------------------------------------

/// Build a bitmap by applying `f` to a contiguous slice, 64 results at a time.
///
/// The shift-OR accumulation into a `u64` is the shape that lets LLVM keep the
/// comparison itself in vector registers: the inner loop has a fixed trip
/// count, no branches and no memory writes.
#[inline]
fn pack<T: Copy>(vals: &[T], mut f: impl FnMut(T) -> bool) -> Bitmap {
    let n = vals.len();
    let mut words = vec![0u64; n.div_ceil(64)];
    for (w, chunk) in vals.chunks(64).enumerate() {
        let mut word = 0u64;
        for (b, v) in chunk.iter().enumerate() {
            word |= (f(*v) as u64) << b;
        }
        words[w] = word;
    }
    Bitmap::from_words(words, n)
}

/// The gathering variant, for a batch that carries a selection.
#[inline]
fn pack_gathered<T: Copy>(vals: &[T], idx: &[u32], mut f: impl FnMut(T) -> bool) -> Bitmap {
    let n = idx.len();
    let mut words = vec![0u64; n.div_ceil(64)];
    for (w, chunk) in idx.chunks(64).enumerate() {
        let mut word = 0u64;
        for (b, i) in chunk.iter().enumerate() {
            word |= (f(vals[*i as usize]) as u64) << b;
        }
        words[w] = word;
    }
    Bitmap::from_words(words, n)
}

/// Build a bitmap from a predicate over logical positions.
///
/// Used where the values are not a flat slice -- offset-encoded strings, packed
/// booleans -- so `pack` does not apply. Still 64 results per word, still no
/// branches in the accumulation.
#[inline]
fn pack_by_index(n: usize, mut f: impl FnMut(usize) -> bool) -> Bitmap {
    let mut words = vec![0u64; n.div_ceil(64)];
    for (w, word) in words.iter_mut().enumerate() {
        let base = w * 64;
        let end = (base + 64).min(n);
        let mut acc = 0u64;
        for i in base..end {
            acc |= (f(i) as u64) << (i - base);
        }
        *word = acc;
    }
    Bitmap::from_words(words, n)
}

/// The comparison operator as a plain function pointer, chosen once outside the
/// loop. `&str` comparison is byte-wise, matching `ScalarValue`'s ordering.
fn str_cmp(op: BinaryOperator) -> fn(&str, &str) -> bool {
    match op {
        BinaryOperator::Eq => |a, b| a == b,
        BinaryOperator::NotEq => |a, b| a != b,
        BinaryOperator::Lt => |a, b| a < b,
        BinaryOperator::LtEq => |a, b| a <= b,
        BinaryOperator::Gt => |a, b| a > b,
        BinaryOperator::GtEq => |a, b| a >= b,
        _ => |_, _| false,
    }
}

fn bool_cmp(op: BinaryOperator) -> fn(bool, bool) -> bool {
    match op {
        BinaryOperator::Eq => |a, b| a == b,
        BinaryOperator::NotEq => |a, b| a != b,
        BinaryOperator::Lt => |a, b| !a & b,
        BinaryOperator::LtEq => |a, b| !a | b,
        BinaryOperator::Gt => |a, b| a & !b,
        BinaryOperator::GtEq => |a, b| a | !b,
        _ => |_, _| false,
    }
}

/// Apply a comparison across a selection, choosing the sequential or gathering
/// form. `rhs` is a closure so the same code serves column-vs-scalar and
/// column-vs-column.
#[inline]
fn compare_over<T: Copy + PartialOrd>(
    vals: &[T],
    sel: &Selection,
    op: BinaryOperator,
    rhs: impl Fn(usize) -> T,
) -> Bitmap {
    // The operator is matched outside the loop so the loop body is one
    // comparison, not a dispatch.
    macro_rules! run {
        ($cmp:expr) => {
            match sel {
                Selection::Range { offset, len } => {
                    let slice = &vals[*offset..*offset + *len];
                    let mut i = 0usize;
                    pack(slice, |v| {
                        let r = rhs(i);
                        i += 1;
                        #[allow(clippy::redundant_closure_call)]
                        ($cmp)(v, r)
                    })
                }
                Selection::Indices(idx) => {
                    let mut i = 0usize;
                    pack_gathered(vals, idx, |v| {
                        let r = rhs(i);
                        i += 1;
                        #[allow(clippy::redundant_closure_call)]
                        ($cmp)(v, r)
                    })
                }
            }
        };
    }
    match op {
        BinaryOperator::Eq => run!(|a, b| a == b),
        BinaryOperator::NotEq => run!(|a: T, b: T| a != b),
        BinaryOperator::Lt => run!(|a: T, b: T| a < b),
        BinaryOperator::LtEq => run!(|a: T, b: T| a <= b),
        BinaryOperator::Gt => run!(|a: T, b: T| a > b),
        BinaryOperator::GtEq => run!(|a: T, b: T| a >= b),
        _ => Bitmap::all_unset(sel.len()),
    }
}

// ---------------------------------------------------------------------------
// Node evaluation
// ---------------------------------------------------------------------------

fn eval_node(expr: &CompiledExpr, batch: &Batch) -> Result<Arc<Column>> {
    let n = batch.num_rows();
    match &expr.node {
        Node::Column(index) => {
            let col = &batch.columns()[*index];
            let sel = batch.selection();
            // An unfiltered batch hands the column straight through.
            if let Selection::Range { offset, len } = sel {
                if *offset == 0 && *len == col.len() {
                    return Ok(Arc::clone(col));
                }
                return Ok(Arc::new(col.slice(*offset, *len)));
            }
            let idx: Vec<usize> = sel.iter().collect();
            Ok(Arc::new(col.take(&idx)))
        }

        Node::Literal(v) => Ok(Arc::new(constant_column(v, n)?)),

        Node::IsNull { expr: inner, negated } => {
            let col = eval_node(inner, batch)?;
            let sel = Selection::full(col.len());
            let valid = Operand { col: &col, sel: &sel }.validity();
            // IS NULL is the one comparison that never yields NULL: it asks
            // *about* nullness rather than propagating it.
            let bits = match valid {
                None => {
                    if *negated {
                        Bitmap::all_set(n)
                    } else {
                        Bitmap::all_unset(n)
                    }
                }
                Some(v) => {
                    if *negated {
                        v
                    } else {
                        v.not()
                    }
                }
            };
            Ok(Arc::new(Column::dense(ColumnData::Boolean(bits))))
        }

        Node::Binary { op, left, right } => eval_binary(expr, *op, left, right, batch),

        Node::Unary { op, expr: inner } => eval_unary(expr, *op, inner, batch),

        Node::Between { expr: inner, low, high, negated } => {
            // `x BETWEEN a AND b` is `x >= a AND x <= b`, NULLs and all, so it
            // reuses the comparison kernels rather than repeating them.
            let ge = compare_columns(BinaryOperator::GtEq, inner, low, batch)?;
            let le = compare_columns(BinaryOperator::LtEq, inner, high, batch)?;
            let inside = boolean_op(BinaryOperator::And, &ge, &le);
            Ok(Arc::new(if *negated { negate(&inside) } else { inside }))
        }

        Node::InList { expr: inner, list, negated } => {
            eval_in_list(inner, list, *negated, batch)
        }

        Node::Like { .. } | Node::Case { .. } | Node::Cast { .. } => {
            eval_row_wise(expr, batch)
        }
    }
}

/// A constant column. Literals are materialized only when a kernel could not
/// take the scalar directly.
fn constant_column(v: &ScalarValue, n: usize) -> Result<Column> {
    if v.is_null() {
        return Ok(Column::new(ColumnData::Null(n), Some(Bitmap::all_unset(n))));
    }
    let data = match v {
        ScalarValue::Boolean(b) => ColumnData::Boolean(if *b {
            Bitmap::all_set(n)
        } else {
            Bitmap::all_unset(n)
        }),
        ScalarValue::Int32(x) => ColumnData::Int32(vec![*x; n]),
        ScalarValue::Int64(x) => ColumnData::Int64(vec![*x; n]),
        ScalarValue::Float64(x) => ColumnData::Float64(vec![*x; n]),
        ScalarValue::Date32(x) => ColumnData::Date32(vec![*x; n]),
        ScalarValue::Timestamp(x) => ColumnData::Timestamp(vec![*x; n]),
        ScalarValue::Decimal128 { value, precision, scale } => ColumnData::Decimal128 {
            values: vec![*value; n],
            precision: *precision,
            scale: *scale,
        },
        ScalarValue::Utf8(s) => {
            let mut sc = StringColumn::with_capacity(n, s.len() * n);
            for _ in 0..n {
                sc.push(s);
            }
            ColumnData::Utf8(sc)
        }
        ScalarValue::Null => unreachable!("handled above"),
    };
    Ok(Column::dense(data))
}

fn eval_binary(
    expr: &CompiledExpr,
    op: BinaryOperator,
    left: &CompiledExpr,
    right: &CompiledExpr,
    batch: &Batch,
) -> Result<Arc<Column>> {
    if op.is_logical() {
        return eval_conjunction(op, left, right, batch);
    }
    if op.is_comparison() {
        return Ok(Arc::new(compare_columns(op, left, right, batch)?));
    }
    let l = eval_node(left, batch)?;
    let r = eval_node(right, batch)?;
    Ok(Arc::new(arithmetic(expr, op, &l, &r)?))
}

fn eval_unary(
    expr: &CompiledExpr,
    op: UnaryOperator,
    inner: &CompiledExpr,
    batch: &Batch,
) -> Result<Arc<Column>> {
    let col = eval_node(inner, batch)?;
    match op {
        UnaryOperator::Plus => Ok(col),
        UnaryOperator::Not => Ok(Arc::new(negate(&col))),
        UnaryOperator::Minus => {
            let data = match &col.data {
                ColumnData::Int32(v) => {
                    let mut out = Vec::with_capacity(v.len());
                    for x in v {
                        out.push(x.checked_neg().ok_or_else(|| overflow(expr))?);
                    }
                    ColumnData::Int32(out)
                }
                ColumnData::Int64(v) => {
                    let mut out = Vec::with_capacity(v.len());
                    for x in v {
                        out.push(x.checked_neg().ok_or_else(|| overflow(expr))?);
                    }
                    ColumnData::Int64(out)
                }
                ColumnData::Float64(v) => ColumnData::Float64(v.iter().map(|x| -x).collect()),
                ColumnData::Null(n) => ColumnData::Null(*n),
                other => {
                    return Err(Diagnostic::exec(format!(
                        "unary `-` is not defined for {}",
                        other.data_type()
                    ))
                    .with_span(expr.span))
                }
            };
            Ok(Arc::new(Column::new(data, col.validity.clone())))
        }
    }
}

/// AND / OR, evaluating the right side only over the rows the left leaves
/// undecided.
///
/// This is not merely an optimization -- it is required for correctness. A
/// guarded expression like `a <> 0 AND 100 / a > 1` must not raise on the rows
/// the first conjunct already rejected, and the scalar evaluator gets that for
/// free by short-circuiting per row. The vectorized equivalent is to build a
/// sub-batch of the still-undecided rows, evaluate the right side there, and
/// scatter the answers back.
///
/// The sub-batch has a cost of its own, so it is only taken when it is needed
/// for correctness or when enough rows are already decided to pay for it.
fn eval_conjunction(
    op: BinaryOperator,
    left: &CompiledExpr,
    right: &CompiledExpr,
    batch: &Batch,
) -> Result<Arc<Column>> {
    let n = batch.num_rows();
    let l = eval_node(left, batch)?;
    let lv = bool_bits(&l, n);
    let lvalid = validity_or_all(&l, n);

    // AND is settled by a FALSE, OR by a TRUE -- in both cases without looking
    // at the other side, and even when the other side is unknown.
    let decided = if op == BinaryOperator::And {
        lvalid.and(&lv.not())
    } else {
        lvalid.and(&lv)
    };
    let undecided = decided.not();
    let remaining = undecided.count_set();

    if remaining == 0 {
        // The right side never runs, so it can never raise.
        return Ok(Arc::new(Column::new(
            ColumnData::Boolean(lv),
            (lvalid.count_set() != n).then_some(lvalid),
        )));
    }

    let worth_narrowing = right.can_raise() || remaining * 2 < n;
    if !worth_narrowing || remaining == n {
        let r = eval_node(right, batch)?;
        return Ok(Arc::new(boolean_op(op, &l, &r)));
    }

    let keep = undecided.set_indices();
    let sub = batch.filter(&keep);
    let r_sub = eval_node(right, &sub)?;

    // Scatter back. Rows that were skipped are marked invalid, which is safe:
    // at a decided row the left side alone determines both the value and the
    // validity of the result, so whatever sits in the right side's slot is
    // ignored by `boolean_op`.
    let mut values = Bitmap::all_unset(n);
    let mut valid = Bitmap::all_unset(n);
    if let ColumnData::Boolean(bits) = &r_sub.data {
        for (j, &pos) in keep.iter().enumerate() {
            if r_sub.is_valid(j) {
                valid.set(pos as usize, true);
                values.set(pos as usize, bits.get(j));
            }
        }
    }
    let r_full = Column::new(ColumnData::Boolean(values), Some(valid));
    Ok(Arc::new(boolean_op(op, &l, &r_full)))
}

/// Three-valued NOT over a boolean column: flip the values, keep the validity.
fn negate(col: &Column) -> Column {
    match &col.data {
        ColumnData::Boolean(b) => Column::new(ColumnData::Boolean(b.not()), col.validity.clone()),
        // NOT of an all-NULL column is still all NULL.
        ColumnData::Null(n) => Column::new(ColumnData::Null(*n), Some(Bitmap::all_unset(*n))),
        _ => col.clone(),
    }
}

/// Three-valued AND / OR, one machine word of rows at a time.
///
/// The validity rule is what makes this more than a bitwise op. For AND, a
/// FALSE settles the answer even against an UNKNOWN, so the result is valid
/// wherever either side is validly false -- not merely where both sides are
/// valid. OR is the mirror image.
fn boolean_op(op: BinaryOperator, l: &Column, r: &Column) -> Column {
    let n = l.len().min(r.len());
    let lv = bool_bits(l, n);
    let rv = bool_bits(r, n);
    let lvalid = validity_or_all(l, n);
    let rvalid = validity_or_all(r, n);

    let (values, valid) = match op {
        BinaryOperator::And => {
            let values = lv.and(&rv);
            // valid = (lvalid & rvalid) | (lvalid & !lv) | (rvalid & !rv)
            let both = lvalid.and(&rvalid);
            let l_false = lvalid.and(&lv.not());
            let r_false = rvalid.and(&rv.not());
            (values, both.or(&l_false).or(&r_false))
        }
        _ => {
            let values = lv.or(&rv);
            let both = lvalid.and(&rvalid);
            let l_true = lvalid.and(&lv);
            let r_true = rvalid.and(&rv);
            (values, both.or(&l_true).or(&r_true))
        }
    };
    // A result that is valid everywhere needs no bitmap.
    let validity = (valid.count_set() != n).then_some(valid);
    Column::new(ColumnData::Boolean(values), validity)
}

fn bool_bits(col: &Column, n: usize) -> Bitmap {
    match &col.data {
        ColumnData::Boolean(b) => b.clone(),
        _ => Bitmap::all_unset(n),
    }
}

fn validity_or_all(col: &Column, n: usize) -> Bitmap {
    match &col.validity {
        Some(v) => v.clone(),
        None => match col.data {
            ColumnData::Null(_) => Bitmap::all_unset(n),
            _ => Bitmap::all_set(n),
        },
    }
}

/// Compare two subexpressions, specializing the common `column OP literal`
/// shape so the column is never materialized.
fn compare_columns(
    op: BinaryOperator,
    left: &CompiledExpr,
    right: &CompiledExpr,
    batch: &Batch,
) -> Result<Column> {
    // Fast path: a leaf column against a constant. This is what almost every
    // WHERE clause looks like, and it reads the row group's own buffer.
    if let (Node::Column(index), Node::Literal(v)) = (&left.node, &right.node) {
        if let Some(col) = compare_scalar(batch.column(*index), batch.selection(), op, v) {
            return Ok(col);
        }
    }
    if let (Node::Literal(v), Node::Column(index)) = (&left.node, &right.node) {
        if let Some(col) = compare_scalar(batch.column(*index), batch.selection(), flip(op), v) {
            return Ok(col);
        }
    }

    let l = eval_node(left, batch)?;
    let r = eval_node(right, batch)?;
    let (lsel, _) = dense(&l);
    let (rsel, _) = dense(&r);
    let lo = Operand { col: &l, sel: &lsel };
    let ro = Operand { col: &r, sel: &rsel };
    Ok(compare_operands(op, lo, ro))
}

/// Mirror an operator so `5 < x` can reuse the `x > 5` kernel.
fn flip(op: BinaryOperator) -> BinaryOperator {
    use BinaryOperator::*;
    match op {
        Lt => Gt,
        LtEq => GtEq,
        Gt => Lt,
        GtEq => LtEq,
        other => other,
    }
}

/// `column OP constant`. Returns `None` for type combinations without a kernel,
/// leaving the caller to fall back to the dense path.
fn compare_scalar(
    col: &Column,
    sel: &Selection,
    op: BinaryOperator,
    rhs: &ScalarValue,
) -> Option<Column> {
    let n = sel.len();
    // Comparing against NULL is UNKNOWN for every row, whatever the values are.
    if rhs.is_null() {
        return Some(Column::new(
            ColumnData::Boolean(Bitmap::all_unset(n)),
            Some(Bitmap::all_unset(n)),
        ));
    }

    let values = match (&col.data, rhs) {
        (ColumnData::Int32(v), ScalarValue::Int32(x)) => {
            compare_over(v, sel, op, |_| *x)
        }
        (ColumnData::Int64(v), ScalarValue::Int64(x)) => {
            compare_over(v, sel, op, |_| *x)
        }
        (ColumnData::Float64(v), ScalarValue::Float64(x)) => {
            compare_over(v, sel, op, |_| *x)
        }
        (ColumnData::Date32(v), ScalarValue::Date32(x)) => {
            compare_over(v, sel, op, |_| *x)
        }
        (ColumnData::Timestamp(v), ScalarValue::Timestamp(x)) => {
            compare_over(v, sel, op, |_| *x)
        }
        // Strings compare straight out of the offset-encoded buffer. Going
        // through `Column::value` here would allocate a `String` per row, which
        // made string predicates slower than the scalar evaluator.
        (ColumnData::Utf8(sc), ScalarValue::Utf8(x)) => {
            let cmp = str_cmp(op);
            let rhs = x.as_str();
            match sel {
                Selection::Range { offset, .. } => {
                    pack_by_index(n, |i| cmp(sc.get(offset + i), rhs))
                }
                Selection::Indices(idx) => {
                    pack_by_index(n, |i| cmp(sc.get(idx[i] as usize), rhs))
                }
            }
        }
        (ColumnData::Boolean(bits), ScalarValue::Boolean(x)) => {
            let cmp = bool_cmp(op);
            match sel {
                Selection::Range { offset, .. } => {
                    pack_by_index(n, |i| cmp(bits.get(offset + i), *x))
                }
                Selection::Indices(idx) => {
                    pack_by_index(n, |i| cmp(bits.get(idx[i] as usize), *x))
                }
            }
        }
        _ => return None,
    };

    let operand = Operand { col, sel };
    let mut validity = operand.validity();
    // NaN is unordered, so any comparison involving it is UNKNOWN -- which is
    // what the scalar evaluator says, and the two must agree.
    if let ColumnData::Float64(v) = &col.data {
        if let Some(nan) = nan_mask(v, sel) {
            validity = combine(validity, Some(nan.not()));
        }
    }
    Some(Column::new(ColumnData::Boolean(values), validity))
}

/// Bits set where a float value is NaN, or `None` when there are none.
fn nan_mask(vals: &[f64], sel: &Selection) -> Option<Bitmap> {
    let mask = match sel {
        Selection::Range { offset, len } => pack(&vals[*offset..*offset + *len], |v| v.is_nan()),
        Selection::Indices(idx) => pack_gathered(vals, idx, |v| v.is_nan()),
    };
    (mask.count_set() > 0).then_some(mask)
}

/// The general comparison, over two operands of the same logical length.
fn compare_operands(op: BinaryOperator, l: Operand<'_>, r: Operand<'_>) -> Column {
    let n = l.len().min(r.len());
    let mut validity = combine(l.validity(), r.validity());

    let values = match (&l.col.data, &r.col.data) {
        (ColumnData::Int32(a), ColumnData::Int32(b)) => {
            let bv = gather(b, r.sel);
            compare_over(a, l.sel, op, |i| bv[i])
        }
        (ColumnData::Int64(a), ColumnData::Int64(b)) => {
            let bv = gather(b, r.sel);
            compare_over(a, l.sel, op, |i| bv[i])
        }
        (ColumnData::Date32(a), ColumnData::Date32(b)) => {
            let bv = gather(b, r.sel);
            compare_over(a, l.sel, op, |i| bv[i])
        }
        (ColumnData::Timestamp(a), ColumnData::Timestamp(b)) => {
            let bv = gather(b, r.sel);
            compare_over(a, l.sel, op, |i| bv[i])
        }
        (ColumnData::Float64(a), ColumnData::Float64(b)) => {
            let bv = gather(b, r.sel);
            let bits = compare_over(a, l.sel, op, |i| bv[i]);
            let mut nan = nan_mask(a, l.sel);
            nan = match (nan, nan_mask(b, r.sel)) {
                (Some(x), Some(y)) => Some(x.or(&y)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
            if let Some(nan) = nan {
                validity = combine(validity, Some(nan.not()));
            }
            bits
        }
        (ColumnData::Boolean(a), ColumnData::Boolean(b)) => {
            let cmp = bool_cmp(op);
            pack_by_index(n, |i| {
                cmp(a.get(l.sel.physical(i)), b.get(r.sel.physical(i)))
            })
        }
        (ColumnData::Utf8(a), ColumnData::Utf8(b)) => {
            let cmp = str_cmp(op);
            pack_by_index(n, |i| {
                cmp(a.get(l.sel.physical(i)), b.get(r.sel.physical(i)))
            })
        }
        // Anything else (strings, decimals, all-NULL columns) goes through the
        // scalar comparison, still once per batch rather than once per node
        // per row.
        _ => {
            let mut words = vec![0u64; n.div_ceil(64)];
            for i in 0..n {
                let a = l.col.value(l.sel.physical(i));
                let b = r.col.value(r.sel.physical(i));
                if let Some(true) = super::scalar::compare_op(op, &a, &b) {
                    words[i / 64] |= 1u64 << (i % 64);
                }
            }
            Bitmap::from_words(words, n)
        }
    };
    Column::new(ColumnData::Boolean(values), validity)
}

/// Materialize a selection of a numeric slice. Only used for the right-hand
/// side of a column-vs-column comparison, where the values must be addressable
/// by logical position.
fn gather<T: Copy>(vals: &[T], sel: &Selection) -> Vec<T> {
    match sel {
        Selection::Range { offset, len } => vals[*offset..*offset + *len].to_vec(),
        Selection::Indices(idx) => idx.iter().map(|i| vals[*i as usize]).collect(),
    }
}

fn arithmetic(
    expr: &CompiledExpr,
    op: BinaryOperator,
    l: &Column,
    r: &Column,
) -> Result<Column> {
    let n = l.len().min(r.len());
    let (lsel, _) = dense(l);
    let (rsel, _) = dense(r);
    let validity = combine(
        Operand { col: l, sel: &lsel }.validity(),
        Operand { col: r, sel: &rsel }.validity(),
    );

    if op == BinaryOperator::StringConcat {
        let mut out = StringColumn::with_capacity(n, 0);
        let mut buf = String::new();
        for i in 0..n {
            buf.clear();
            if let (ScalarValue::Utf8(a), ScalarValue::Utf8(b)) = (l.value(i), r.value(i)) {
                buf.push_str(&a);
                buf.push_str(&b);
            }
            out.push(&buf);
        }
        return Ok(Column::new(ColumnData::Utf8(out), validity));
    }

    // Which slots hold a real value. Arithmetic still runs over NULL slots (they
    // contain a harmless placeholder), but an overflow or a division by zero
    // there must not raise: the row's answer is NULL either way.
    let live = |i: usize| validity.as_ref().is_none_or(|v| v.get(i));

    let data = match (&l.data, &r.data) {
        (ColumnData::Int32(a), ColumnData::Int32(b)) => {
            ColumnData::Int32(int_arith(a, b, n, op, expr, &live)?)
        }
        (ColumnData::Int64(a), ColumnData::Int64(b)) => {
            ColumnData::Int64(int_arith(a, b, n, op, expr, &live)?)
        }
        (ColumnData::Float64(a), ColumnData::Float64(b)) => {
            if matches!(op, BinaryOperator::Divide | BinaryOperator::Modulo) {
                // One vectorizable pass to find a zero divisor, so the division
                // loop itself stays branch-free.
                for (i, divisor) in b[..n].iter().enumerate() {
                    if *divisor == 0.0 && live(i) {
                        return Err(Diagnostic::exec("division by zero").with_span(expr.span));
                    }
                }
            }
            let out: Vec<f64> = match op {
                BinaryOperator::Plus => a[..n].iter().zip(&b[..n]).map(|(x, y)| x + y).collect(),
                BinaryOperator::Minus => a[..n].iter().zip(&b[..n]).map(|(x, y)| x - y).collect(),
                BinaryOperator::Multiply => a[..n].iter().zip(&b[..n]).map(|(x, y)| x * y).collect(),
                BinaryOperator::Divide => a[..n].iter().zip(&b[..n]).map(|(x, y)| x / y).collect(),
                BinaryOperator::Modulo => a[..n].iter().zip(&b[..n]).map(|(x, y)| x % y).collect(),
                _ => return Err(bad_operator(op, expr)),
            };
            ColumnData::Float64(out)
        }
        (ColumnData::Null(_), _) | (_, ColumnData::Null(_)) => ColumnData::Null(n),
        _ => {
            return Err(Diagnostic::exec(format!(
                "operator `{}` is not defined for {} and {}",
                op.as_str(),
                l.data_type(),
                r.data_type()
            ))
            .with_span(expr.span))
        }
    };
    Ok(Column::new(data, validity))
}

/// Integer arithmetic uses checked operations, which LLVM will not vectorize.
/// That is the deliberate trade: silently wrapping would be wrong, and integer
/// arithmetic is far colder than the comparisons that dominate a filter.
fn int_arith<T>(
    a: &[T],
    b: &[T],
    n: usize,
    op: BinaryOperator,
    expr: &CompiledExpr,
    live: &impl Fn(usize) -> bool,
) -> Result<Vec<T>>
where
    T: Copy + Default + CheckedArith,
{
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if !live(i) {
            // The value is NULL; nothing may be raised on its behalf.
            out.push(T::default());
            continue;
        }
        let v = match op {
            BinaryOperator::Plus => a[i].checked_add(b[i]),
            BinaryOperator::Minus => a[i].checked_sub(b[i]),
            BinaryOperator::Multiply => a[i].checked_mul(b[i]),
            BinaryOperator::Divide => {
                if b[i].eq_zero() {
                    return Err(Diagnostic::exec("division by zero").with_span(expr.span));
                }
                a[i].checked_div(b[i])
            }
            BinaryOperator::Modulo => {
                if b[i].eq_zero() {
                    return Err(Diagnostic::exec("division by zero").with_span(expr.span));
                }
                a[i].checked_rem(b[i])
            }
            _ => return Err(bad_operator(op, expr)),
        };
        out.push(v.ok_or_else(|| overflow(expr))?);
    }
    Ok(out)
}

trait CheckedArith: Sized {
    fn checked_add(self, other: Self) -> Option<Self>;
    fn checked_sub(self, other: Self) -> Option<Self>;
    fn checked_mul(self, other: Self) -> Option<Self>;
    fn checked_div(self, other: Self) -> Option<Self>;
    fn checked_rem(self, other: Self) -> Option<Self>;
    fn eq_zero(self) -> bool;
}

macro_rules! impl_checked {
    ($t:ty) => {
        impl CheckedArith for $t {
            fn checked_add(self, o: Self) -> Option<Self> {
                <$t>::checked_add(self, o)
            }
            fn checked_sub(self, o: Self) -> Option<Self> {
                <$t>::checked_sub(self, o)
            }
            fn checked_mul(self, o: Self) -> Option<Self> {
                <$t>::checked_mul(self, o)
            }
            fn checked_div(self, o: Self) -> Option<Self> {
                <$t>::checked_div(self, o)
            }
            fn checked_rem(self, o: Self) -> Option<Self> {
                <$t>::checked_rem(self, o)
            }
            fn eq_zero(self) -> bool {
                self == 0
            }
        }
    };
}
impl_checked!(i32);
impl_checked!(i64);

fn eval_in_list(
    inner: &CompiledExpr,
    list: &[CompiledExpr],
    negated: bool,
    batch: &Batch,
) -> Result<Arc<Column>> {
    let n = batch.num_rows();
    // An empty set settles the answer before the value matters: `x IN ()` is
    // FALSE and `x NOT IN ()` is TRUE, even for a NULL x, because there is
    // nothing for it to fail to match. Reachable once a subquery supplies the
    // list.
    if list.is_empty() {
        let bits = if negated {
            Bitmap::all_set(n)
        } else {
            Bitmap::all_unset(n)
        };
        return Ok(Arc::new(Column::dense(ColumnData::Boolean(bits))));
    }
    let value = eval_node(inner, batch)?;
    let (vsel, _) = dense(&value);
    let value_valid = Operand { col: &value, sel: &vsel }.validity();

    let mut matched = Bitmap::all_unset(n);
    let mut list_has_null = false;
    for item in list {
        // An IN list is almost always literals. Comparing against the scalar
        // directly avoids building a constant column per item per batch.
        let eq = if let Node::Literal(v) = &item.node {
            if v.is_null() {
                list_has_null = true;
                continue;
            }
            match compare_scalar(&value, &vsel, BinaryOperator::Eq, v) {
                Some(col) => col,
                None => compare_operands(
                    BinaryOperator::Eq,
                    Operand { col: &value, sel: &vsel },
                    Operand {
                        col: &constant_column(v, n)?,
                        sel: &Selection::full(n),
                    },
                ),
            }
        } else {
            let item_col = eval_node(item, batch)?;
            let (isel, _) = dense(&item_col);
            let item_op = Operand { col: &item_col, sel: &isel };
            if item_op.validity().is_some_and(|v| v.count_set() < n) {
                list_has_null = true;
            }
            compare_operands(
                BinaryOperator::Eq,
                Operand { col: &value, sel: &vsel },
                item_op,
            )
        };
        let bits = match (&eq.data, &eq.validity) {
            (ColumnData::Boolean(b), Some(valid)) => b.and(valid),
            (ColumnData::Boolean(b), None) => b.clone(),
            _ => Bitmap::all_unset(n),
        };
        matched = matched.or(&bits);
    }

    // The NOT IN trap: with no match but a NULL in the list, the answer is
    // UNKNOWN rather than FALSE, so `x NOT IN (1, NULL)` returns no rows at all
    // instead of "every row where x <> 1".
    let valid = if list_has_null {
        // Only a positive match is known; everything else is unknown.
        matched.clone()
    } else {
        Bitmap::all_set(n)
    };
    let valid = combine(Some(valid), value_valid).expect("always Some");

    let values = if negated { matched.not() } else { matched };
    let validity = (valid.count_set() != n).then_some(valid);
    Ok(Arc::new(Column::new(
        ColumnData::Boolean(values),
        validity,
    )))
}

/// The fallback for expressions with no kernel yet -- CAST, LIKE and CASE.
///
/// Still batch-at-a-time in the sense that matters: the tree is walked once,
/// column indices are already resolved, and the output is built directly into a
/// typed column. Only the innermost step is per-row.
fn eval_row_wise(expr: &CompiledExpr, batch: &Batch) -> Result<Arc<Column>> {
    let n = batch.num_rows();
    match &expr.node {
        Node::Cast { expr: inner } => {
            let src = eval_node(inner, batch)?;
            let mut builder = ColumnBuilder::new(&expr.data_type);
            for i in 0..n {
                let v = types::cast_scalar(&src.value(i), &expr.data_type)
                    .map_err(|d| d.with_span(expr.span))?;
                builder.append(&v)?;
            }
            Ok(Arc::new(builder.finish()))
        }

        Node::Like {
            expr: inner, pattern, escape, negated, case_insensitive,
        } => {
            let text = eval_node(inner, batch)?;
            let pat = eval_node(pattern, batch)?;
            let esc = match escape {
                Some(e) => Some(eval_node(e, batch)?),
                None => None,
            };

            // The overwhelmingly common case is a constant pattern, so hoist it
            // out of the loop rather than re-reading it per row.
            let constant_pattern = match &pattern.node {
                Node::Literal(ScalarValue::Utf8(p)) => Some(p.clone()),
                _ => None,
            };

            let mut values = Bitmap::with_capacity(n);
            let mut validity = Bitmap::with_capacity(n);
            let mut any_null = false;
            for i in 0..n {
                let t = text.value(i);
                let p = match &constant_pattern {
                    Some(p) => ScalarValue::Utf8(p.clone()),
                    None => pat.value(i),
                };
                let e = esc.as_ref().map(|c| c.value(i));
                if t.is_null() || p.is_null() || e.as_ref().is_some_and(|v| v.is_null()) {
                    any_null = true;
                    validity.push(false);
                    values.push(false);
                    continue;
                }
                let (ScalarValue::Utf8(t), ScalarValue::Utf8(p)) = (&t, &p) else {
                    return Err(Diagnostic::exec("internal: LIKE operands must be strings")
                        .with_span(expr.span));
                };
                let escape_char = match &e {
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
                validity.push(true);
                values.push(like_match(t, p, *case_insensitive, escape_char) != *negated);
            }
            Ok(Arc::new(Column::new(
                ColumnData::Boolean(values),
                any_null.then_some(validity),
            )))
        }

        Node::Case { when_then, else_expr } => {
            // Branches are evaluated only over the rows that reach them, one
            // narrowing sub-batch at a time. That is not an optimization: a row
            // that does not take a branch must not be affected by it, so
            // `CASE WHEN x <> 0 THEN 1 / x ELSE 0 END` has to work. Evaluating
            // every branch over the whole batch would divide by zero on exactly
            // the rows the guard excludes.
            //
            // Conditions narrow the same way, so a condition after one that
            // already matched is never evaluated for that row either.
            let mut assignment: Vec<Option<(usize, usize)>> = vec![None; n];
            let mut remaining: Vec<u32> = (0..n as u32).collect();
            let mut branch_values: Vec<Option<Arc<Column>>> = Vec::with_capacity(when_then.len());

            for (branch, (cond, result)) in when_then.iter().enumerate() {
                if remaining.is_empty() {
                    branch_values.push(None);
                    continue;
                }
                let undecided = batch.filter(&remaining);
                let taken_local = eval_predicate(cond, &undecided)?;
                if taken_local.is_empty() {
                    branch_values.push(None);
                    continue;
                }
                // Positions inside `undecided` map back through `remaining`.
                let taken: Vec<u32> = taken_local
                    .iter()
                    .map(|i| remaining[*i as usize])
                    .collect();
                let values = eval_node(result, &batch.filter(&taken))?;
                for (position, row) in taken.iter().enumerate() {
                    assignment[*row as usize] = Some((branch, position));
                }
                branch_values.push(Some(values));

                let mut is_taken = vec![false; n];
                for row in &taken {
                    is_taken[*row as usize] = true;
                }
                remaining.retain(|r| !is_taken[*r as usize]);
            }

            // Whatever is left takes the ELSE, or NULL if there is none.
            let otherwise = match (else_expr, remaining.is_empty()) {
                (Some(e), false) => Some(eval_node(e, &batch.filter(&remaining))?),
                _ => None,
            };

            let mut builder = ColumnBuilder::new(&expr.data_type);
            let mut else_position = 0usize;
            for slot in assignment.iter().take(n) {
                match *slot {
                    Some((branch, position)) => {
                        let values = branch_values[branch]
                            .as_ref()
                            .expect("a taken branch produced values");
                        builder.append(&values.value(position))?;
                    }
                    None => match &otherwise {
                        // `remaining` stayed in ascending row order, so the
                        // ELSE column's rows line up as they are consumed.
                        Some(c) => {
                            builder.append(&c.value(else_position))?;
                            else_position += 1;
                        }
                        None => builder.append_null(),
                    },
                }
            }
            Ok(Arc::new(builder.finish()))
        }

        _ => unreachable!("eval_row_wise called on a node with a kernel"),
    }
}

fn overflow(expr: &CompiledExpr) -> Diagnostic {
    Diagnostic::exec("arithmetic overflow").with_span(expr.span)
}

fn bad_operator(op: BinaryOperator, expr: &CompiledExpr) -> Diagnostic {
    Diagnostic::exec(format!(
        "internal: `{}` is not an arithmetic operator",
        op.as_str()
    ))
    .with_span(expr.span)
}
