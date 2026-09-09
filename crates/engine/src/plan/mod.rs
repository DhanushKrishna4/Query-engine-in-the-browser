//! Logical plan and bound (typed) expressions.
//!
//! Representation: an owned tree with `Box` children, where every relation
//! carries a `RelId` and every expression an `ExprId`. Rewrite rules will be
//! `fn(LogicalPlan) -> Option<LogicalPlan>` applied bottom-up, rebuilding the
//! tree, and the optimizer trace will be a list of whole before/after clones.
//! Plans here are tens of nodes, so a clone costs microseconds -- and the UI
//! wants complete trees anyway, so there is no arena-to-tree step to write.
//!
//! Every `BoundExpr` knows its `data_type` and its nullability. That is the
//! whole point of the binder: after this stage nothing downstream has to ask
//! "what type is this?" again, and nullability tells the optimizer when a
//! rewrite that changes NULL behaviour is safe.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::error::Span;
use crate::parser::ast::{BinaryOperator, JoinOperator, UnaryOperator};
use crate::storage::Schema;
use crate::types::{DataType, ScalarValue};

/// Unique identifier for a relation (a scan, or any operator that produces a
/// new relation). Lets the optimizer talk about "the column at index 2 of
/// relation 3" without depending on where that relation sits in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelId(pub u32);

/// Unique identifier for an expression. Used later for common subexpression
/// elimination and for keying UI state to a specific node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

impl std::fmt::Display for RelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct BoundExpr {
    pub id: ExprId,
    pub kind: BoundExprKind,
    pub data_type: DataType,
    /// Whether this expression can evaluate to NULL. Conservative: `true` when
    /// unsure. Rewrites that depend on NULL-rejection read this.
    pub nullable: bool,
    /// Byte range in the original SQL, so runtime errors can point at the
    /// subexpression that failed.
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum BoundExprKind {
    /// Column `index` of relation `rel`. The index is relative to that
    /// relation's own output, not to the batch flowing through any particular
    /// operator; execution maps `rel` to a batch offset. That indirection is
    /// what will let join reordering move relations around without rewriting
    /// every column reference.
    Column {
        rel: RelId,
        index: usize,
        name: String,
    },
    Literal(ScalarValue),
    Binary {
        op: BinaryOperator,
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
    },
    Unary {
        op: UnaryOperator,
        expr: Box<BoundExpr>,
    },
    /// Target type is this expression's own `data_type`.
    Cast {
        expr: Box<BoundExpr>,
        /// True when the binder inserted this cast to satisfy a coercion rule,
        /// rather than the user writing `CAST(...)`. The distinction matters in
        /// two places: an implicit cast is hidden from user-facing renderings
        /// (it should not turn up in a default output column name), and a
        /// rewrite rule may remove one it can prove redundant, which it must
        /// never do to a cast the user asked for.
        implicit: bool,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    Between {
        expr: Box<BoundExpr>,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        escape: Option<Box<BoundExpr>>,
        negated: bool,
        case_insensitive: bool,
    },
    /// Always the searched form. A simple `CASE x WHEN v` is rewritten into
    /// `CASE WHEN x = v` during binding.
    Case {
        when_then: Vec<(BoundExpr, BoundExpr)>,
        else_expr: Option<Box<BoundExpr>>,
    },
    /// A subquery used as a value or a predicate.
    ///
    /// The plan is held as-is rather than being folded into a join by the
    /// binder, so that decorrelation is a *rewrite* the optimizer performs and
    /// the trace can show -- and so that a query still runs correctly with the
    /// optimizer switched off.
    ///
    /// The plan may reference relations that are not inside it: those are the
    /// correlated columns, supplied by the row being evaluated.
    Subquery {
        kind: SubqueryKind,
        plan: Arc<LogicalPlan>,
    },
}

#[derive(Debug, Clone)]
pub enum SubqueryKind {
    /// `(SELECT ...)` as a value: NULL if it returns no row, an error if it
    /// returns more than one.
    Scalar,
    /// `[NOT] EXISTS (...)`.
    Exists { negated: bool },
    /// `expr [NOT] IN (...)`.
    In {
        expr: Box<BoundExpr>,
        negated: bool,
    },
}

impl SubqueryKind {
    pub fn label(&self) -> &'static str {
        match self {
            SubqueryKind::Scalar => "SUBQUERY",
            SubqueryKind::Exists { negated: false } => "EXISTS",
            SubqueryKind::Exists { negated: true } => "NOT EXISTS",
            SubqueryKind::In { negated: false, .. } => "IN",
            SubqueryKind::In { negated: true, .. } => "NOT IN",
        }
    }
}

impl BoundExpr {
    /// Every relation this expression reads from.
    ///
    /// Used by join planning to decide which side of a join a conjunct belongs
    /// to: an equality whose two halves touch disjoint sides is an equi-join
    /// key, and anything else is a residual condition.
    pub fn relations(&self, out: &mut std::collections::BTreeSet<RelId>) {
        match &self.kind {
            BoundExprKind::Column { rel, .. } => {
                out.insert(*rel);
            }
            BoundExprKind::Literal(_) => {}
            BoundExprKind::Binary { left, right, .. } => {
                left.relations(out);
                right.relations(out);
            }
            BoundExprKind::Unary { expr, .. }
            | BoundExprKind::Cast { expr, .. }
            | BoundExprKind::IsNull { expr, .. } => expr.relations(out),
            BoundExprKind::Between { expr, low, high, .. } => {
                expr.relations(out);
                low.relations(out);
                high.relations(out);
            }
            BoundExprKind::InList { expr, list, .. } => {
                expr.relations(out);
                for e in list {
                    e.relations(out);
                }
            }
            BoundExprKind::Like { expr, pattern, escape, .. } => {
                expr.relations(out);
                pattern.relations(out);
                if let Some(e) = escape {
                    e.relations(out);
                }
            }
            BoundExprKind::Case { when_then, else_expr } => {
                for (w, t) in when_then {
                    w.relations(out);
                    t.relations(out);
                }
                if let Some(e) = else_expr {
                    e.relations(out);
                }
            }
            BoundExprKind::Subquery { kind, plan } => {
                // A subquery's own relations count as referenced by whatever
                // holds it, which is what makes a correlated reference visible
                // from the outside.
                plan.collect_referenced(out);
                if let SubqueryKind::In { expr, .. } = kind {
                    expr.relations(out);
                }
            }
        }
    }

    /// The first subquery expression at or below this node, if any.
    pub fn find_subquery(&self) -> Option<&BoundExpr> {
        if matches!(self.kind, BoundExprKind::Subquery { .. }) {
            return Some(self);
        }
        match &self.kind {
            BoundExprKind::Binary { left, right, .. } => {
                left.find_subquery().or_else(|| right.find_subquery())
            }
            BoundExprKind::Unary { expr, .. }
            | BoundExprKind::Cast { expr, .. }
            | BoundExprKind::IsNull { expr, .. } => expr.find_subquery(),
            BoundExprKind::Between { expr, low, high, .. } => expr
                .find_subquery()
                .or_else(|| low.find_subquery())
                .or_else(|| high.find_subquery()),
            BoundExprKind::InList { expr, list, .. } => expr
                .find_subquery()
                .or_else(|| list.iter().find_map(|i| i.find_subquery())),
            BoundExprKind::Like { expr, pattern, escape, .. } => expr
                .find_subquery()
                .or_else(|| pattern.find_subquery())
                .or_else(|| escape.as_ref().and_then(|e| e.find_subquery())),
            BoundExprKind::Case { when_then, else_expr } => when_then
                .iter()
                .find_map(|(w, t)| w.find_subquery().or_else(|| t.find_subquery()))
                .or_else(|| else_expr.as_ref().and_then(|e| e.find_subquery())),
            _ => None,
        }
    }

    pub fn relation_set(&self) -> std::collections::BTreeSet<RelId> {
        let mut out = std::collections::BTreeSet::new();
        self.relations(&mut out);
        out
    }

    /// Split a predicate on its top-level `AND`s. Join planning works conjunct
    /// by conjunct, since each may belong to a different side.
    pub fn split_conjuncts(self, out: &mut Vec<BoundExpr>) {
        match self.kind {
            BoundExprKind::Binary {
                op: BinaryOperator::And,
                left,
                right,
            } => {
                left.split_conjuncts(out);
                right.split_conjuncts(out);
            }
            _ => out.push(self),
        }
    }

    /// Whether evaluating this subtree could raise: division by zero, integer
    /// overflow, or a failing CAST.
    ///
    /// Conservative on purpose. It gates rewrites that drop an operand --
    /// simplifying `x AND false` to `false` is only sound if evaluating `x`
    /// could not have raised, or the optimizer would be turning a query that
    /// errors into one that does not.
    pub fn can_raise(&self) -> bool {
        match &self.kind {
            BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => false,
            BoundExprKind::Cast { .. } => true,
            BoundExprKind::Binary { op, left, right } => {
                op.is_arithmetic() || left.can_raise() || right.can_raise()
            }
            BoundExprKind::Unary { op, expr } => {
                *op == UnaryOperator::Minus || expr.can_raise()
            }
            BoundExprKind::IsNull { expr, .. } => expr.can_raise(),
            BoundExprKind::Between { expr, low, high, .. } => {
                expr.can_raise() || low.can_raise() || high.can_raise()
            }
            BoundExprKind::InList { expr, list, .. } => {
                expr.can_raise() || list.iter().any(|e| e.can_raise())
            }
            BoundExprKind::Like { expr, pattern, escape, .. } => {
                expr.can_raise() || pattern.can_raise() || escape.is_some()
            }
            BoundExprKind::Case { when_then, else_expr } => {
                when_then.iter().any(|(w, t)| w.can_raise() || t.can_raise())
                    || else_expr.as_ref().is_some_and(|e| e.can_raise())
            }
            // A scalar subquery raises if it returns more than one row.
            BoundExprKind::Subquery { .. } => true,
        }
    }

    /// Combine predicates with `AND`, or `None` for an empty list.
    pub fn and_all(mut parts: Vec<BoundExpr>) -> Option<BoundExpr> {
        let id = parts.first()?.id;
        if parts.len() == 1 {
            return parts.pop();
        }
        parts.into_iter().reduce(|a, b| BoundExpr {
            id,
            data_type: DataType::Boolean,
            nullable: a.nullable || b.nullable,
            span: a.span.merge(b.span),
            kind: BoundExprKind::Binary {
                op: BinaryOperator::And,
                left: Box::new(a),
                right: Box::new(b),
            },
        })
    }

    /// SQL-like rendering, e.g. `b > 5`.
    pub fn to_sql(&self) -> String {
        self.render(false)
    }

    /// The same, with every leaf's resolved type attached: `b::INT64 > 5::INT64`.
    /// This is what the UI's BOUND PLAN tab shows.
    pub fn to_typed_sql(&self) -> String {
        self.render(true)
    }

    fn render(&self, typed: bool) -> String {
        let mut s = match &self.kind {
            BoundExprKind::Column { name, rel, .. } => {
                if typed {
                    format!("{rel}.{name}")
                } else {
                    name.clone()
                }
            }
            BoundExprKind::Literal(v) => match v {
                ScalarValue::Utf8(t) => format!("'{t}'"),
                other => other.to_string(),
            },
            BoundExprKind::Binary { op, left, right } => format!(
                "({} {} {})",
                left.render(typed),
                op.as_str(),
                right.render(typed)
            ),
            BoundExprKind::Unary { op, expr } => match op {
                UnaryOperator::Not => format!("(NOT {})", expr.render(typed)),
                other => format!("({}{})", other.as_str(), expr.render(typed)),
            },
            BoundExprKind::Cast { expr, implicit } => {
                if *implicit && !typed {
                    // Untyped rendering is what names output columns and
                    // labels operators, so it shows what the user wrote.
                    expr.render(typed)
                } else {
                    format!("CAST({} AS {})", expr.render(typed), self.data_type)
                }
            }
            BoundExprKind::IsNull { expr, negated } => format!(
                "({} IS {}NULL)",
                expr.render(typed),
                if *negated { "NOT " } else { "" }
            ),
            BoundExprKind::Between { expr, low, high, negated } => format!(
                "({} {}BETWEEN {} AND {})",
                expr.render(typed),
                if *negated { "NOT " } else { "" },
                low.render(typed),
                high.render(typed)
            ),
            BoundExprKind::InList { expr, list, negated } => {
                let items: Vec<String> = list.iter().map(|e| e.render(typed)).collect();
                format!(
                    "({} {}IN ({}))",
                    expr.render(typed),
                    if *negated { "NOT " } else { "" },
                    items.join(", ")
                )
            }
            BoundExprKind::Like {
                expr, pattern, escape, negated, case_insensitive,
            } => {
                let mut s = format!(
                    "({} {}{} {}",
                    expr.render(typed),
                    if *negated { "NOT " } else { "" },
                    if *case_insensitive { "ILIKE" } else { "LIKE" },
                    pattern.render(typed)
                );
                if let Some(e) = escape {
                    let _ = write!(s, " ESCAPE {}", e.render(typed));
                }
                s.push(')');
                s
            }
            BoundExprKind::Subquery { kind, plan } => {
                let inner = plan.describe(false);
                match kind {
                    SubqueryKind::In { expr, negated } => format!(
                        "({} {}IN <{}>)",
                        expr.render(typed),
                        if *negated { "NOT " } else { "" },
                        inner
                    ),
                    other => format!("{} <{inner}>", other.label()),
                }
            }
            BoundExprKind::Case { when_then, else_expr } => {
                let mut s = String::from("CASE");
                for (w, t) in when_then {
                    let _ = write!(s, " WHEN {} THEN {}", w.render(typed), t.render(typed));
                }
                if let Some(e) = else_expr {
                    let _ = write!(s, " ELSE {}", e.render(typed));
                }
                s.push_str(" END");
                s
            }
        };
        if typed && matches!(self.kind, BoundExprKind::Column { .. } | BoundExprKind::Literal(_)) {
            let _ = write!(s, "::{}", self.data_type);
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    /// Left rows are preserved: one with no match is emitted padded with NULLs
    /// on the right.
    Left,
    Right,
    Full,
    Cross,
    /// Emit each left row that has at least one match, once, with only the left
    /// columns. What `EXISTS` and `IN` become.
    Semi,
    /// Emit each left row that has *no* match. What `NOT EXISTS` becomes -- and
    /// what `NOT IN` becomes only when neither side can be NULL, because a NULL
    /// makes `NOT IN` unknown rather than true.
    Anti,
}

impl JoinType {
    pub fn from_ast(op: JoinOperator) -> JoinType {
        match op {
            JoinOperator::Inner => JoinType::Inner,
            JoinOperator::Left => JoinType::Left,
            JoinOperator::Right => JoinType::Right,
            JoinOperator::Full => JoinType::Full,
            JoinOperator::Cross => JoinType::Cross,
        }
    }

    /// Whether rows of the left input survive without a match.
    pub fn preserves_left(&self) -> bool {
        matches!(self, JoinType::Left | JoinType::Full)
    }

    /// Whether the output is the left input's columns alone.
    pub fn is_filtering(&self) -> bool {
        matches!(self, JoinType::Semi | JoinType::Anti)
    }

    /// Whether rows of the right input survive without a match.
    pub fn preserves_right(&self) -> bool {
        matches!(self, JoinType::Right | JoinType::Full)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JoinType::Inner => "INNER",
            JoinType::Left => "LEFT",
            JoinType::Right => "RIGHT",
            JoinType::Full => "FULL",
            JoinType::Cross => "CROSS",
            JoinType::Semi => "SEMI",
            JoinType::Anti => "ANTI",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunction {
    pub fn from_name(name: &str) -> Option<AggregateFunction> {
        Some(match name.to_ascii_lowercase().as_str() {
            "count" => AggregateFunction::Count,
            "sum" => AggregateFunction::Sum,
            "avg" => AggregateFunction::Avg,
            "min" => AggregateFunction::Min,
            "max" => AggregateFunction::Max,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AggregateFunction::Count => "COUNT",
            AggregateFunction::Sum => "SUM",
            AggregateFunction::Avg => "AVG",
            AggregateFunction::Min => "MIN",
            AggregateFunction::Max => "MAX",
        }
    }
}

/// One aggregate in an `Aggregate` node's output.
#[derive(Debug, Clone)]
pub struct BoundAggregate {
    pub func: AggregateFunction,
    /// `None` for `COUNT(*)`, which counts rows rather than values.
    pub arg: Option<BoundExpr>,
    pub distinct: bool,
    pub data_type: DataType,
    pub nullable: bool,
    pub span: Span,
}

impl BoundAggregate {
    pub fn to_sql(&self) -> String {
        let arg = match &self.arg {
            None => "*".to_string(),
            Some(e) => e.to_sql(),
        };
        format!(
            "{}({}{arg})",
            self.func.as_str(),
            if self.distinct { "DISTINCT " } else { "" }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunction {
    /// Position in the partition, 1-based, never tied.
    RowNumber,
    /// Position with ties sharing a rank and the next rank skipping ahead.
    Rank,
    /// Position with ties sharing a rank and no gaps.
    DenseRank,
    Lag,
    Lead,
    /// An ordinary aggregate computed over the frame instead of over a group.
    Aggregate(AggregateFunction),
}

impl WindowFunction {
    pub fn from_name(name: &str) -> Option<WindowFunction> {
        Some(match name.to_ascii_lowercase().as_str() {
            "row_number" => WindowFunction::RowNumber,
            "rank" => WindowFunction::Rank,
            "dense_rank" => WindowFunction::DenseRank,
            "lag" => WindowFunction::Lag,
            "lead" => WindowFunction::Lead,
            other => WindowFunction::Aggregate(AggregateFunction::from_name(other)?),
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            WindowFunction::RowNumber => "ROW_NUMBER",
            WindowFunction::Rank => "RANK",
            WindowFunction::DenseRank => "DENSE_RANK",
            WindowFunction::Lag => "LAG",
            WindowFunction::Lead => "LEAD",
            WindowFunction::Aggregate(a) => a.as_str(),
        }
    }

    /// Ranking functions describe a row's position, so a frame would mean
    /// nothing to them.
    pub fn ignores_frame(&self) -> bool {
        !matches!(self, WindowFunction::Aggregate(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnits {
    /// Counted in rows, so tied rows have different frames.
    Rows,
    /// Counted in values: rows equal on the ORDER BY keys are peers and share
    /// a frame. This is what makes `SUM(x) OVER (ORDER BY y)` give every row
    /// with the same `y` the same running total.
    Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(usize),
    CurrentRow,
    Following(usize),
    UnboundedFollowing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
}

impl Frame {
    /// The frame a window with no `ROWS`/`RANGE` clause gets.
    ///
    /// With an ORDER BY it is everything up to and including the current row's
    /// peers, which is what makes an ordered aggregate a running total. Without
    /// one there is no order to run along, so the frame is the whole partition.
    pub fn default_for(ordered: bool) -> Frame {
        if ordered {
            Frame {
                units: FrameUnits::Range,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::CurrentRow,
            }
        } else {
            Frame {
                units: FrameUnits::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::UnboundedFollowing,
            }
        }
    }

    pub fn is_whole_partition(&self) -> bool {
        self.start == FrameBound::UnboundedPreceding
            && self.end == FrameBound::UnboundedFollowing
    }
}

#[derive(Debug, Clone)]
pub struct BoundWindowFunction {
    pub func: WindowFunction,
    pub args: Vec<BoundExpr>,
    pub frame: Frame,
    pub data_type: DataType,
    pub nullable: bool,
    /// How the column is named in the output.
    pub label: String,
}

/// One key of a sort, with the direction and NULL placement resolved.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: BoundExpr,
    pub ascending: bool,
    /// Where NULLs go. SQL leaves this implementation-defined when the query
    /// does not say; this engine follows SQLite -- NULLs first ascending, last
    /// descending -- because SQLite is what the differential corpus compares
    /// against. PostgreSQL chose the opposite default.
    pub nulls_first: bool,
}

impl SortKey {
    pub fn to_sql(&self) -> String {
        format!(
            "{} {} NULLS {}",
            self.expr.to_sql(),
            if self.ascending { "ASC" } else { "DESC" },
            if self.nulls_first { "FIRST" } else { "LAST" }
        )
    }
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// One row, zero columns. The input to a FROM-less `SELECT 1 + 1`: the
    /// projection needs something to evaluate against exactly once.
    OneRow { rel: RelId },
    Scan {
        rel: RelId,
        table_name: String,
        /// The table's full schema. Column references index into this, and keep
        /// indexing into it however much the projection prunes.
        table_schema: Arc<Schema>,
        /// Which of the table's columns the scan actually reads, in output
        /// order. `None` means all of them.
        projection: Option<Vec<usize>>,
        /// `table_schema` restricted to `projection`.
        schema: Arc<Schema>,
    },
    Filter {
        rel: RelId,
        predicate: BoundExpr,
        input: Box<LogicalPlan>,
    },
    Project {
        rel: RelId,
        exprs: Vec<BoundExpr>,
        schema: Arc<Schema>,
        input: Box<LogicalPlan>,
    },
    Limit {
        rel: RelId,
        skip: usize,
        /// `None` means "no upper bound", i.e. OFFSET without LIMIT.
        fetch: Option<usize>,
        input: Box<LogicalPlan>,
    },
    Join {
        rel: RelId,
        join_type: JoinType,
        /// `None` only for a cross join.
        on: Option<BoundExpr>,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        /// Left columns then right columns, with nullability widened on
        /// whichever side the join type does not preserve.
        schema: Arc<Schema>,
    },
    Aggregate {
        rel: RelId,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        /// Grouping columns first, then aggregate results.
        schema: Arc<Schema>,
        input: Box<LogicalPlan>,
    },
    /// Names a subquery's output so its columns can be referenced.
    ///
    /// A projection produces anonymous columns; this gives them a relation to
    /// belong to, which is what makes `FROM (SELECT ...) x` addressable as
    /// `x.col` and what lets a decorrelated subquery appear on one side of a
    /// join.
    SubqueryAlias {
        rel: RelId,
        alias: String,
        schema: Arc<Schema>,
        input: Box<LogicalPlan>,
    },
    /// A reference to a `WITH` binding.
    ///
    /// The definition is behind an `Arc` shared by every reference to the same
    /// CTE, which is what lets the executor compute it once however many times
    /// it is named. It is deliberately **not** a child: a rule that pushed a
    /// predicate from one reference into the shared definition would change
    /// what the *other* references see, so the definition is opaque to
    /// rewrites and optimized on its own.
    ///
    /// The cost of that opacity is real and worth naming: `WITH t AS (SELECT *
    /// FROM big) SELECT * FROM t WHERE x > 5` cannot push the filter into the
    /// CTE, where inlining the definition at each use would. Postgres made the
    /// same trade for twenty years before adding a heuristic to inline
    /// single-reference CTEs, which is the obvious next step here.
    CteRef {
        rel: RelId,
        name: String,
        definition: Arc<LogicalPlan>,
        schema: Arc<Schema>,
    },
    Sort {
        rel: RelId,
        keys: Vec<SortKey>,
        input: Box<LogicalPlan>,
    },
    /// Remove duplicate rows. NULLs count as equal to each other here, as they
    /// do for grouping and unlike a comparison.
    Distinct {
        rel: RelId,
        input: Box<LogicalPlan>,
    },
    /// Compute one value per row over a window of its neighbours.
    ///
    /// Unlike an aggregate, this adds columns rather than collapsing rows: the
    /// output is the input's columns followed by one per function.
    Window {
        rel: RelId,
        partition_by: Vec<BoundExpr>,
        order_by: Vec<SortKey>,
        functions: Vec<BoundWindowFunction>,
        schema: Arc<Schema>,
        input: Box<LogicalPlan>,
    },
    SetOp {
        rel: RelId,
        op: SetOperator,
        /// `true` keeps duplicates. UNION defaults to removing them, and so do
        /// INTERSECT and EXCEPT.
        all: bool,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        schema: Arc<Schema>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

impl SetOperator {
    pub fn as_str(&self) -> &'static str {
        match self {
            SetOperator::Union => "UNION",
            SetOperator::Intersect => "INTERSECT",
            SetOperator::Except => "EXCEPT",
        }
    }
}

impl LogicalPlan {
    pub fn rel(&self) -> RelId {
        match self {
            LogicalPlan::OneRow { rel }
            | LogicalPlan::Scan { rel, .. }
            | LogicalPlan::Filter { rel, .. }
            | LogicalPlan::Project { rel, .. }
            | LogicalPlan::Limit { rel, .. }
            | LogicalPlan::Join { rel, .. }
            | LogicalPlan::SubqueryAlias { rel, .. }
            | LogicalPlan::Sort { rel, .. }
            | LogicalPlan::Window { rel, .. }
            | LogicalPlan::Distinct { rel, .. }
            | LogicalPlan::SetOp { rel, .. }
            | LogicalPlan::CteRef { rel, .. }
            | LogicalPlan::Aggregate { rel, .. } => *rel,
        }
    }

    pub fn schema(&self) -> Arc<Schema> {
        match self {
            LogicalPlan::OneRow { .. } => Arc::new(Schema::empty()),
            LogicalPlan::CteRef { schema, .. }
            | LogicalPlan::Scan { schema, .. }
            | LogicalPlan::Project { schema, .. }
            | LogicalPlan::Join { schema, .. }
            | LogicalPlan::SubqueryAlias { schema, .. }
            | LogicalPlan::SetOp { schema, .. }
            | LogicalPlan::Window { schema, .. }
            | LogicalPlan::Aggregate { schema, .. } => Arc::clone(schema),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Distinct { input, .. } => input.schema(),
        }
    }

    /// Rebuild this node around new children. The count must match what
    /// `children` returned; rewrite rules rely on that to walk generically.
    pub fn with_children(&self, mut children: Vec<LogicalPlan>) -> LogicalPlan {
        debug_assert_eq!(children.len(), self.children().len());
        match self {
            LogicalPlan::OneRow { .. }
            | LogicalPlan::Scan { .. }
            // A CTE reference has no children to rebuild: its definition is
            // shared with every other reference, so no rule may rewrite it
            // through one of them.
            | LogicalPlan::CteRef { .. } => self.clone(),
            LogicalPlan::Filter { rel, predicate, .. } => LogicalPlan::Filter {
                rel: *rel,
                predicate: predicate.clone(),
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::Project { rel, exprs, schema, .. } => LogicalPlan::Project {
                rel: *rel,
                exprs: exprs.clone(),
                schema: Arc::clone(schema),
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::Limit { rel, skip, fetch, .. } => LogicalPlan::Limit {
                rel: *rel,
                skip: *skip,
                fetch: *fetch,
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::Aggregate { rel, group_exprs, aggregates, schema, .. } => {
                LogicalPlan::Aggregate {
                    rel: *rel,
                    group_exprs: group_exprs.clone(),
                    aggregates: aggregates.clone(),
                    schema: Arc::clone(schema),
                    input: Box::new(children.remove(0)),
                }
            }
            LogicalPlan::Sort { rel, keys, .. } => LogicalPlan::Sort {
                rel: *rel,
                keys: keys.clone(),
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::Window { rel, partition_by, order_by, functions, .. } => {
                let input = children.remove(0);
                // Derived from the input, so pruning a column below has to be
                // reflected here -- the same rule as a join's schema.
                let schema = window_schema(functions, &input);
                LogicalPlan::Window {
                    rel: *rel,
                    partition_by: partition_by.clone(),
                    order_by: order_by.clone(),
                    functions: functions.clone(),
                    schema,
                    input: Box::new(input),
                }
            }
            LogicalPlan::Distinct { rel, .. } => LogicalPlan::Distinct {
                rel: *rel,
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::SetOp { rel, op, all, schema, .. } => {
                let left = children.remove(0);
                let right = children.remove(0);
                LogicalPlan::SetOp {
                    rel: *rel,
                    op: *op,
                    all: *all,
                    left: Box::new(left),
                    right: Box::new(right),
                    schema: Arc::clone(schema),
                }
            }
            LogicalPlan::SubqueryAlias { rel, alias, schema, .. } => LogicalPlan::SubqueryAlias {
                rel: *rel,
                alias: alias.clone(),
                schema: Arc::clone(schema),
                input: Box::new(children.remove(0)),
            },
            LogicalPlan::Join { rel, join_type, on, .. } => {
                let left = children.remove(0);
                let right = children.remove(0);
                // The schema is derived from the children, so pruning a column
                // out of a scan below has to be reflected here.
                let schema = join_schema(*join_type, &left, &right);
                LogicalPlan::Join {
                    rel: *rel,
                    join_type: *join_type,
                    on: on.clone(),
                    left: Box::new(left),
                    right: Box::new(right),
                    schema,
                }
            }
        }
    }

    /// Relations whose columns are addressable in this plan's output.
    pub fn relations(&self) -> std::collections::BTreeSet<RelId> {
        let mut out = std::collections::BTreeSet::new();
        self.collect_relations(&mut out);
        out
    }

    fn collect_relations(&self, out: &mut std::collections::BTreeSet<RelId>) {
        match self {
            LogicalPlan::Scan { rel, .. }
            | LogicalPlan::CteRef { rel, .. }
            | LogicalPlan::Aggregate { rel, .. }
            | LogicalPlan::SubqueryAlias { rel, .. } => {
                out.insert(*rel);
            }
            // A projection produces new anonymous columns, so nothing above it
            // addresses the relations underneath by id.
            LogicalPlan::OneRow { .. } | LogicalPlan::Project { .. } => {}
            // A set operation produces new rows that belong to neither input,
            // so nothing above it can address either by relation.
            LogicalPlan::SetOp { .. } => {}
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Distinct { input, .. } => input.collect_relations(out),
            // A window keeps its input's columns and adds its own, so both are
            // addressable above it.
            LogicalPlan::Window { rel, input, .. } => {
                input.collect_relations(out);
                out.insert(*rel);
            }
            LogicalPlan::Join { left, right, .. } => {
                left.collect_relations(out);
                right.collect_relations(out);
            }
        }
    }

    pub fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::OneRow { .. }
            | LogicalPlan::Scan { .. }
            | LogicalPlan::CteRef { .. } => Vec::new(),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::SubqueryAlias { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Window { input, .. }
            | LogicalPlan::Distinct { input, .. }
            | LogicalPlan::Limit { input, .. } => vec![input],
            LogicalPlan::Join { left, right, .. } => vec![left, right],
            LogicalPlan::SetOp { left, right, .. } => vec![left, right],
        }
    }

    /// One-line description of this node alone.
    pub fn describe(&self, typed: bool) -> String {
        match self {
            LogicalPlan::OneRow { .. } => "OneRow".to_string(),
            LogicalPlan::CteRef { name, .. } => format!("CteRef {name}"),
            LogicalPlan::Scan { table_name, table_schema, schema, .. } => {
                let cols: Vec<String> = schema
                    .fields
                    .iter()
                    .map(|f| format!("{}:{}", f.name, f.data_type))
                    .collect();
                let pruned = table_schema.len() - schema.len();
                format!(
                    "Scan table={table_name} columns=[{}]{}",
                    cols.join(", "),
                    if pruned > 0 {
                        format!(" ({pruned} column(s) pruned)")
                    } else {
                        String::new()
                    }
                )
            }
            LogicalPlan::Filter { predicate, .. } => format!(
                "Filter {}",
                if typed { predicate.to_typed_sql() } else { predicate.to_sql() }
            ),
            LogicalPlan::Project { exprs, schema, .. } => {
                let items: Vec<String> = exprs
                    .iter()
                    .zip(&schema.fields)
                    .map(|(e, f)| {
                        let rendered = if typed { e.to_typed_sql() } else { e.to_sql() };
                        if rendered == f.name {
                            if typed {
                                format!("{rendered} -> {}:{}", f.name, f.data_type)
                            } else {
                                rendered
                            }
                        } else {
                            format!("{rendered} AS {}", f.name)
                        }
                    })
                    .collect();
                format!("Project [{}]", items.join(", "))
            }
            LogicalPlan::Limit { skip, fetch, .. } => match fetch {
                Some(n) if *skip > 0 => format!("Limit fetch={n} skip={skip}"),
                Some(n) => format!("Limit fetch={n}"),
                None => format!("Limit skip={skip}"),
            },
            LogicalPlan::Join { join_type, on, .. } => match on {
                Some(c) => format!(
                    "Join {} on {}",
                    join_type.as_str(),
                    if typed { c.to_typed_sql() } else { c.to_sql() }
                ),
                None => format!("Join {}", join_type.as_str()),
            },
            LogicalPlan::SubqueryAlias { alias, .. } => format!("SubqueryAlias {alias}"),
            LogicalPlan::Sort { keys, .. } => {
                let text: Vec<String> = keys.iter().map(|k| k.to_sql()).collect();
                format!("Sort [{}]", text.join(", "))
            }
            LogicalPlan::Distinct { .. } => "Distinct".to_string(),
            LogicalPlan::Window { partition_by, order_by, functions, .. } => {
                let names: Vec<String> = functions.iter().map(|f| f.label.clone()).collect();
                let mut text = format!("Window [{}]", names.join(", "));
                if !partition_by.is_empty() {
                    let keys: Vec<String> = partition_by.iter().map(|e| e.to_sql()).collect();
                    text.push_str(&format!(" partition=[{}]", keys.join(", ")));
                }
                if !order_by.is_empty() {
                    let keys: Vec<String> = order_by.iter().map(|k| k.to_sql()).collect();
                    text.push_str(&format!(" order=[{}]", keys.join(", ")));
                }
                text
            }
            LogicalPlan::SetOp { op, all, .. } => {
                format!("{}{}", op.as_str(), if *all { " ALL" } else { "" })
            }
            LogicalPlan::Aggregate { group_exprs, aggregates, .. } => {
                let groups: Vec<String> = group_exprs
                    .iter()
                    .map(|e| if typed { e.to_typed_sql() } else { e.to_sql() })
                    .collect();
                let aggs: Vec<String> = aggregates.iter().map(|a| a.to_sql()).collect();
                if groups.is_empty() {
                    format!("Aggregate [{}]", aggs.join(", "))
                } else {
                    format!("Aggregate group=[{}] [{}]", groups.join(", "), aggs.join(", "))
                }
            }
        }
    }
}

impl BoundExpr {
    /// Rewrite this expression bottom-up.
    ///
    /// `f` is offered every node after its children have been rewritten;
    /// returning `Some` replaces it. Used by constant folding, by correlated
    /// column substitution, and by the subquery passes.
    pub fn rewrite(&self, f: &mut impl FnMut(&BoundExpr) -> Option<BoundExpr>) -> BoundExpr {
        let kind = match &self.kind {
            BoundExprKind::Column { .. } | BoundExprKind::Literal(_) => {
                return f(self).unwrap_or_else(|| self.clone())
            }
            BoundExprKind::Binary { op, left, right } => BoundExprKind::Binary {
                op: *op,
                left: Box::new(left.rewrite(f)),
                right: Box::new(right.rewrite(f)),
            },
            BoundExprKind::Unary { op, expr } => BoundExprKind::Unary {
                op: *op,
                expr: Box::new(expr.rewrite(f)),
            },
            BoundExprKind::Cast { expr, implicit } => BoundExprKind::Cast {
                expr: Box::new(expr.rewrite(f)),
                implicit: *implicit,
            },
            BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
                expr: Box::new(expr.rewrite(f)),
                negated: *negated,
            },
            BoundExprKind::Between { expr, low, high, negated } => BoundExprKind::Between {
                expr: Box::new(expr.rewrite(f)),
                low: Box::new(low.rewrite(f)),
                high: Box::new(high.rewrite(f)),
                negated: *negated,
            },
            BoundExprKind::InList { expr, list, negated } => BoundExprKind::InList {
                expr: Box::new(expr.rewrite(f)),
                list: list.iter().map(|i| i.rewrite(f)).collect(),
                negated: *negated,
            },
            BoundExprKind::Like { expr, pattern, escape, negated, case_insensitive } => {
                BoundExprKind::Like {
                    expr: Box::new(expr.rewrite(f)),
                    pattern: Box::new(pattern.rewrite(f)),
                    escape: escape.as_ref().map(|x| Box::new(x.rewrite(f))),
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                }
            }
            BoundExprKind::Case { when_then, else_expr } => BoundExprKind::Case {
                when_then: when_then
                    .iter()
                    .map(|(w, t)| (w.rewrite(f), t.rewrite(f)))
                    .collect(),
                else_expr: else_expr.as_ref().map(|x| Box::new(x.rewrite(f))),
            },
            // The subquery's own plan is not descended into: it is a separate
            // query, rewritten on its own terms.
            BoundExprKind::Subquery { .. } => return f(self).unwrap_or_else(|| self.clone()),
        };
        let rebuilt = BoundExpr { kind, ..self.clone() };
        f(&rebuilt).unwrap_or(rebuilt)
    }
}

impl LogicalPlan {
    /// Rewrite every expression in this plan and, recursively, its children.
    pub fn rewrite_expressions(
        &self,
        f: &mut impl FnMut(&BoundExpr) -> Option<BoundExpr>,
    ) -> LogicalPlan {
        let children: Vec<LogicalPlan> = self
            .children()
            .into_iter()
            .map(|c| c.rewrite_expressions(f))
            .collect();
        let rebuilt = self.with_children(children);
        match &rebuilt {
            LogicalPlan::Filter { rel, predicate, input } => LogicalPlan::Filter {
                rel: *rel,
                predicate: predicate.rewrite(f),
                input: input.clone(),
            },
            LogicalPlan::Project { rel, exprs, schema, input } => LogicalPlan::Project {
                rel: *rel,
                exprs: exprs.iter().map(|e| e.rewrite(f)).collect(),
                schema: Arc::clone(schema),
                input: input.clone(),
            },
            LogicalPlan::Join { rel, join_type, on, left, right, schema } => LogicalPlan::Join {
                rel: *rel,
                join_type: *join_type,
                on: on.as_ref().map(|c| c.rewrite(f)),
                left: left.clone(),
                right: right.clone(),
                schema: Arc::clone(schema),
            },
            LogicalPlan::Sort { rel, keys, input } => LogicalPlan::Sort {
                rel: *rel,
                keys: keys
                    .iter()
                    .map(|k| SortKey {
                        expr: k.expr.rewrite(f),
                        ..*k
                    })
                    .collect(),
                input: input.clone(),
            },
            LogicalPlan::Aggregate { rel, group_exprs, aggregates, schema, input } => {
                LogicalPlan::Aggregate {
                    rel: *rel,
                    group_exprs: group_exprs.iter().map(|e| e.rewrite(f)).collect(),
                    aggregates: aggregates
                        .iter()
                        .map(|a| {
                            let mut a = a.clone();
                            a.arg = a.arg.as_ref().map(|e| e.rewrite(f));
                            a
                        })
                        .collect(),
                    schema: Arc::clone(schema),
                    input: input.clone(),
                }
            }
            _ => rebuilt,
        }
    }

    /// The first subquery expression still present anywhere in this plan.
    ///
    /// Used as a post-condition: the mandatory rules must remove every one, so
    /// anything left is reported rather than reaching an executor that cannot
    /// run it.
    pub fn find_subquery(&self) -> Option<&BoundExpr> {
        let own: Option<&BoundExpr> = match self {
            LogicalPlan::Filter { predicate, .. } => predicate.find_subquery(),
            LogicalPlan::Project { exprs, .. } => exprs.iter().find_map(|e| e.find_subquery()),
            LogicalPlan::Join { on, .. } => on.as_ref().and_then(|c| c.find_subquery()),
            LogicalPlan::Aggregate { group_exprs, aggregates, .. } => group_exprs
                .iter()
                .find_map(|e| e.find_subquery())
                .or_else(|| {
                    aggregates
                        .iter()
                        .find_map(|a| a.arg.as_ref().and_then(|e| e.find_subquery()))
                }),
            _ => None,
        };
        own.or_else(|| self.children().into_iter().find_map(|c| c.find_subquery()))
    }

    /// Visit every expression this node owns (not its children's).
    pub fn for_each_expr(&self, f: &mut impl FnMut(&BoundExpr)) {
        match self {
            LogicalPlan::OneRow { .. }
            | LogicalPlan::CteRef { .. }
            | LogicalPlan::Scan { .. }
            | LogicalPlan::Limit { .. }
            | LogicalPlan::Distinct { .. }
            | LogicalPlan::SetOp { .. }
            | LogicalPlan::SubqueryAlias { .. } => {}
            LogicalPlan::Sort { keys, .. } => keys.iter().for_each(|k| f(&k.expr)),
            LogicalPlan::Window { partition_by, order_by, functions, .. } => {
                partition_by.iter().for_each(&mut *f);
                order_by.iter().for_each(|k| f(&k.expr));
                for w in functions {
                    w.args.iter().for_each(&mut *f);
                }
            }
            LogicalPlan::Filter { predicate, .. } => f(predicate),
            LogicalPlan::Project { exprs, .. } => exprs.iter().for_each(&mut *f),
            LogicalPlan::Join { on, .. } => {
                if let Some(c) = on {
                    f(c);
                }
            }
            LogicalPlan::Aggregate { group_exprs, aggregates, .. } => {
                group_exprs.iter().for_each(&mut *f);
                for a in aggregates {
                    if let Some(e) = &a.arg {
                        f(e);
                    }
                }
            }
        }
    }

    /// Every relation *defined* anywhere inside this plan, whether or not it
    /// still reaches the output.
    pub fn defined_relations(&self) -> std::collections::BTreeSet<RelId> {
        let mut out = std::collections::BTreeSet::new();
        self.collect_defined(&mut out);
        out
    }

    fn collect_defined(&self, out: &mut std::collections::BTreeSet<RelId>) {
        if matches!(
            self,
            LogicalPlan::Scan { .. }
                | LogicalPlan::Aggregate { .. }
                | LogicalPlan::SubqueryAlias { .. }
        ) {
            out.insert(self.rel());
        }
        for c in self.children() {
            c.collect_defined(out);
        }
    }

    /// Every relation any expression in this plan reads from, including through
    /// nested subqueries.
    pub fn referenced_relations(&self) -> std::collections::BTreeSet<RelId> {
        let mut out = std::collections::BTreeSet::new();
        self.collect_referenced(&mut out);
        out
    }

    fn collect_referenced(&self, out: &mut std::collections::BTreeSet<RelId>) {
        self.for_each_expr(&mut |e| e.relations(out));
        for c in self.children() {
            c.collect_referenced(out);
        }
    }

    /// Relations this plan reads but does not define: the correlated ones,
    /// supplied by whatever row the enclosing query is evaluating.
    pub fn correlated_relations(&self) -> std::collections::BTreeSet<RelId> {
        let defined = self.defined_relations();
        self.referenced_relations()
            .into_iter()
            .filter(|r| !defined.contains(r))
            .collect()
    }

    pub fn is_correlated(&self) -> bool {
        !self.correlated_relations().is_empty()
    }
}

/// Left columns then right columns, with nullability widened on whichever side
/// the join type does not preserve.
///
/// This is the fact every outer-join rewrite rule depends on: a LEFT join can
/// emit a row whose right-hand columns are all NULL even when the base columns
/// are declared NOT NULL.
pub fn join_schema(join_type: JoinType, left: &LogicalPlan, right: &LogicalPlan) -> Arc<Schema> {
    // A semi or anti join filters the left input rather than combining the two,
    // so the right side contributes no columns at all.
    if join_type.is_filtering() {
        return left.schema();
    }
    let ls = left.schema();
    let rs = right.schema();
    let mut fields = Vec::with_capacity(ls.len() + rs.len());
    for f in &ls.fields {
        let mut f = f.clone();
        f.nullable |= join_type.preserves_right();
        fields.push(f);
    }
    for f in &rs.fields {
        let mut f = f.clone();
        f.nullable |= join_type.preserves_left();
        fields.push(f);
    }
    Arc::new(Schema::new(fields))
}

/// A window's output: its input's columns, then one per function.
pub fn window_schema(functions: &[BoundWindowFunction], input: &LogicalPlan) -> Arc<Schema> {
    let mut fields = input.schema().fields.clone();
    for f in functions {
        fields.push(crate::storage::Field::new(
            f.label.clone(),
            f.data_type,
            f.nullable,
        ));
    }
    Arc::new(Schema::new(fields))
}

/// Largest relation id anywhere in the plan, so new nodes can be given ids that
/// do not collide.
pub fn max_rel_id(plan: &LogicalPlan) -> u32 {
    let mut max = plan.rel().0;
    for c in plan.children() {
        max = max.max(max_rel_id(c));
    }
    max
}

/// Render the plan as an indented tree. `typed` adds resolved types to every
/// leaf, which is what makes the bound plan worth looking at.
pub fn explain(plan: &LogicalPlan, typed: bool) -> String {
    let mut out = String::new();
    write_plan(&mut out, plan, 0, typed);
    out
}

fn write_plan(out: &mut String, plan: &LogicalPlan, depth: usize, typed: bool) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    if depth > 0 {
        out.push_str("-> ");
    }
    out.push_str(&plan.describe(typed));
    if typed {
        let _ = write!(out, "   [{}]", plan.rel());
    }
    out.push('\n');
    for child in plan.children() {
        write_plan(out, child, depth + 1, typed);
    }
}
