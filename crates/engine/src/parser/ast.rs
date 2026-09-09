//! The abstract syntax tree.
//!
//! Deliberately *untyped* and close to the source text: numeric literals stay
//! as strings, `(a)` keeps its parentheses, and identifiers remember whether
//! they were quoted. Everything the binder needs to make a decision is still
//! here, and the UI's AST tab can render something that looks like what the
//! user typed.
//!
//! `Select` carries slots for every clause in the target grammar even though
//! the parser currently only fills some of them. That keeps adding GROUP BY or
//! joins an additive change rather than a reshape of every downstream match.

use std::fmt::Write as _;

use crate::error::Span;
use crate::types::DataType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub value: String,
    /// A `"quoted"` identifier is case-sensitive and matched verbatim; a bare
    /// one is folded to lowercase during binding.
    pub quoted: bool,
    pub span: Span,
}

impl Ident {
    /// The form used for catalog and scope lookups: bare identifiers fold to
    /// lowercase (as in PostgreSQL), quoted ones keep their case.
    pub fn normalized(&self) -> String {
        if self.quoted {
            self.value.clone()
        } else {
            self.value.to_ascii_lowercase()
        }
    }
}

/// A parsed statement. Only SELECT exists today; the enum is here so that the
/// eventual `EXPLAIN`/`CREATE` forms slot in without changing signatures.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// `WITH name AS (...)` definitions, in the order written. A later one may
    /// reference an earlier one; none may reference itself, because
    /// `RECURSIVE` is not part of this subset.
    pub with: Vec<Cte>,
    pub body: SetExpr,
    /// Applies to the whole body, so a set operation is sorted after being
    /// combined rather than each branch being sorted separately.
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub span: Span,
}

impl Query {
    /// The single SELECT this query is, if it is not a set operation.
    pub fn as_select(&self) -> Option<&Select> {
        match &self.body {
            SetExpr::Select(s) => Some(s),
            SetExpr::SetOp { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetExpr {
    Select(Box<Select>),
    SetOp {
        op: SetOperator,
        /// `true` for the `ALL` form, which keeps duplicates.
        all: bool,
        left: Box<SetExpr>,
        right: Box<SetExpr>,
        span: Span,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    /// Comma-separated FROM items, each with its own chain of joins. A comma
    /// between items is a cross join, which is how the two forms unify.
    pub from: Vec<FromItem>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub span: Span,
}

/// One entry in the FROM clause: a relation plus everything joined onto it.
/// Joins associate to the left, so `a JOIN b JOIN c` is `(a JOIN b) JOIN c`.
#[derive(Debug, Clone, PartialEq)]
pub struct FromItem {
    pub relation: TableRef,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub operator: JoinOperator,
    pub relation: TableRef,
    pub constraint: JoinConstraint,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOperator {
    Inner,
    /// Every row of the left input is preserved, padded with NULLs when it has
    /// no match.
    Left,
    Right,
    Full,
    Cross,
}

impl JoinOperator {
    pub fn as_str(&self) -> &'static str {
        match self {
            JoinOperator::Inner => "INNER",
            JoinOperator::Left => "LEFT",
            JoinOperator::Right => "RIGHT",
            JoinOperator::Full => "FULL",
            JoinOperator::Cross => "CROSS",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinConstraint {
    On(Expr),
    /// `USING (a, b)`. Parsed so it can be rejected by name; it is not just
    /// sugar for ON, because it also merges the paired columns in `SELECT *`.
    Using(Vec<Ident>),
    /// A cross join, or a comma between FROM items.
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard { span: Span },
    QualifiedWildcard { qualifier: Ident, span: Span },
    Expr { expr: Expr, alias: Option<Ident> },
}

impl SelectItem {
    pub fn span(&self) -> Span {
        match self {
            SelectItem::Wildcard { span } | SelectItem::QualifiedWildcard { span, .. } => *span,
            SelectItem::Expr { expr, alias } => match alias {
                Some(a) => expr.span().merge(a.span),
                None => expr.span(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub factor: TableFactor,
    pub alias: Option<Ident>,
    pub span: Span,
}

/// One `WITH` binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: Ident,
    /// `WITH t(a, b) AS (...)` renames the query's output columns. Empty when
    /// the names come from the query itself.
    pub columns: Vec<Ident>,
    pub query: Query,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableFactor {
    Table(Ident),
    /// `(SELECT ...) alias` -- a derived table. The alias is required: without
    /// one there is no way to qualify its columns.
    Derived(Box<Query>),
}

impl TableRef {
    /// The name this relation answers to as a qualifier.
    pub fn binding(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.factor {
            TableFactor::Table(name) => Some(name),
            TableFactor::Derived(_) => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByExpr {
    pub expr: Expr,
    /// `None` means unspecified, which defaults to ASC.
    pub asc: Option<bool>,
    /// `None` means unspecified; the default depends on direction.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Plus, Minus, Multiply, Divide, Modulo,
    Eq, NotEq, Lt, LtEq, Gt, GtEq,
    And, Or,
    /// `||`
    StringConcat,
}

impl BinaryOperator {
    pub fn as_str(&self) -> &'static str {
        use BinaryOperator::*;
        match self {
            Plus => "+", Minus => "-", Multiply => "*", Divide => "/", Modulo => "%",
            Eq => "=", NotEq => "<>", Lt => "<", LtEq => "<=", Gt => ">", GtEq => ">=",
            And => "AND", Or => "OR", StringConcat => "||",
        }
    }

    pub fn is_comparison(&self) -> bool {
        use BinaryOperator::*;
        matches!(self, Eq | NotEq | Lt | LtEq | Gt | GtEq)
    }

    pub fn is_logical(&self) -> bool {
        matches!(self, BinaryOperator::And | BinaryOperator::Or)
    }

    pub fn is_arithmetic(&self) -> bool {
        use BinaryOperator::*;
        matches!(self, Plus | Minus | Multiply | Divide | Modulo)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Plus,
    Minus,
    Not,
}

impl UnaryOperator {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnaryOperator::Plus => "+",
            UnaryOperator::Minus => "-",
            UnaryOperator::Not => "NOT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Null,
    Boolean(bool),
    /// Kept as written. Choosing Int64 vs Float64 is a typing decision and
    /// therefore belongs to the binder, not the parser.
    Number(String),
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Identifier(Ident),
    /// `t.a`, or in principle `db.t.a`.
    CompoundIdentifier(Vec<Ident>),
    Literal { value: Literal, span: Span },
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOperator,
        right: Box<Expr>,
        op_span: Span,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<Expr>,
        span: Span,
    },
    /// Explicit parentheses, preserved so the AST view mirrors the source.
    Nested { expr: Box<Expr>, span: Span },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
        span: Span,
    },
    Between {
        expr: Box<Expr>,
        negated: bool,
        low: Box<Expr>,
        high: Box<Expr>,
        span: Span,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
        span: Span,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
        escape: Option<Box<Expr>>,
        span: Span,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
        span: Span,
    },
    Case {
        /// `CASE x WHEN ...` keeps `x` here; searched `CASE WHEN ...` has None.
        operand: Option<Box<Expr>>,
        when_then: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
        span: Span,
    },
    Function {
        name: Ident,
        args: Vec<FunctionArg>,
        distinct: bool,
        /// `OVER (...)` makes this a window function: it produces one value per
        /// row rather than collapsing rows into groups.
        over: Option<WindowSpec>,
        span: Span,
    },
    /// `(SELECT ...)` used as a value. Must produce one column, and at most one
    /// row per evaluation.
    ScalarSubquery { query: Box<Query>, span: Span },
    /// `[NOT] EXISTS (SELECT ...)`.
    Exists {
        query: Box<Query>,
        negated: bool,
        span: Span,
    },
    /// `expr [NOT] IN (SELECT ...)`. Kept apart from `InList` because the NULL
    /// rules are the same but the execution strategy is not.
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
        span: Span,
    },
}

/// The `OVER (...)` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub frame: Option<WindowFrame>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrame {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnits {
    /// Counted in rows, so ties are separate.
    Rows,
    /// Counted in values, so rows equal on the ORDER BY keys share a frame.
    Range,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArg {
    /// The `*` in `COUNT(*)`.
    Wildcard(Span),
    Expr(Expr),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Identifier(i) => i.span,
            Expr::CompoundIdentifier(parts) => parts
                .first()
                .map(|f| f.span.merge(parts.last().unwrap().span))
                .unwrap_or_default(),
            Expr::Literal { span, .. }
            | Expr::UnaryOp { span, .. }
            | Expr::Nested { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::Between { span, .. }
            | Expr::InList { span, .. }
            | Expr::Like { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Case { span, .. }
            | Expr::Function { span, .. }
            | Expr::ScalarSubquery { span, .. }
            | Expr::Exists { span, .. }
            | Expr::InSubquery { span, .. } => *span,
            Expr::BinaryOp { left, right, .. } => left.span().merge(right.span()),
        }
    }
}

// ---------------------------------------------------------------------------
// Pretty printing
// ---------------------------------------------------------------------------

/// Render the AST as an indented tree. This is the exact text the UI's AST tab
/// shows, and it is what the parser snapshot tests assert on -- a change in the
/// tree shape shows up as a readable diff rather than a `Debug` blob.
pub fn pretty(stmt: &Statement) -> String {
    let mut out = String::new();
    match stmt {
        Statement::Query(q) => write_query(&mut out, q, 0),
    }
    out
}

fn line(out: &mut String, depth: usize, text: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(text);
    out.push('\n');
}

fn write_query(out: &mut String, q: &Query, d: usize) {
    line(out, d, "Query");
    write_set_expr(out, &q.body, d + 1);
    if !q.order_by.is_empty() {
        line(out, d + 1, "OrderBy");
        for o in &q.order_by {
            let dir = match o.asc {
                Some(true) | None => "ASC",
                Some(false) => "DESC",
            };
            let nulls = match o.nulls_first {
                Some(true) => " NULLS FIRST",
                Some(false) => " NULLS LAST",
                None => "",
            };
            line(out, d + 2, &format!("{dir}{nulls}"));
            write_expr(out, &o.expr, d + 3);
        }
    }
    if let Some(l) = &q.limit {
        line(out, d + 1, "Limit");
        write_expr(out, l, d + 2);
    }
    if let Some(o) = &q.offset {
        line(out, d + 1, "Offset");
        write_expr(out, o, d + 2);
    }
}

fn write_table_ref(out: &mut String, t: &TableRef, d: usize) {
    match &t.factor {
        TableFactor::Table(name) => {
            let text = match &t.alias {
                Some(a) => format!("Table {} AS {}", name.value, a.value),
                None => format!("Table {}", name.value),
            };
            line(out, d, &text);
        }
        TableFactor::Derived(query) => {
            let alias = t.alias.as_ref().map(|a| a.value.as_str()).unwrap_or("?");
            line(out, d, &format!("Derived AS {alias}"));
            write_query(out, query, d + 1);
        }
    }
}

fn bound_text(b: &FrameBound) -> String {
    match b {
        FrameBound::UnboundedPreceding => "unbounded preceding".into(),
        FrameBound::CurrentRow => "current row".into(),
        FrameBound::UnboundedFollowing => "unbounded following".into(),
        FrameBound::Preceding(_) => "n preceding".into(),
        FrameBound::Following(_) => "n following".into(),
    }
}

fn write_set_expr(out: &mut String, e: &SetExpr, d: usize) {
    match e {
        SetExpr::Select(s) => write_select(out, s, d),
        SetExpr::SetOp { op, all, left, right, .. } => {
            line(
                out,
                d,
                &format!("{}{}", op.as_str(), if *all { " ALL" } else { "" }),
            );
            write_set_expr(out, left, d + 1);
            write_set_expr(out, right, d + 1);
        }
    }
}

fn write_select(out: &mut String, s: &Select, d: usize) {
    line(out, d, if s.distinct { "Select distinct" } else { "Select" });

    line(out, d + 1, "Projection");
    for item in &s.projection {
        match item {
            SelectItem::Wildcard { .. } => line(out, d + 2, "*"),
            SelectItem::QualifiedWildcard { qualifier, .. } => {
                line(out, d + 2, &format!("{}.*", qualifier.value))
            }
            SelectItem::Expr { expr, alias } => {
                match alias {
                    Some(a) => line(out, d + 2, &format!("Alias {}", a.value)),
                    None => line(out, d + 2, "Item"),
                }
                write_expr(out, expr, d + 3);
            }
        }
    }

    if !s.from.is_empty() {
        line(out, d + 1, "From");
        for item in &s.from {
            write_table_ref(out, &item.relation, d + 2);
            for j in &item.joins {
                line(out, d + 2, &format!("{} join", j.operator.as_str()));
                write_table_ref(out, &j.relation, d + 3);
                match &j.constraint {
                    JoinConstraint::On(e) => {
                        line(out, d + 3, "on");
                        write_expr(out, e, d + 4);
                    }
                    JoinConstraint::Using(cols) => {
                        let names: Vec<&str> = cols.iter().map(|c| c.value.as_str()).collect();
                        line(out, d + 3, &format!("using ({})", names.join(", ")));
                    }
                    JoinConstraint::None => {}
                }
            }
        }
    }

    if let Some(w) = &s.selection {
        line(out, d + 1, "Where");
        write_expr(out, w, d + 2);
    }
    if !s.group_by.is_empty() {
        line(out, d + 1, "GroupBy");
        for g in &s.group_by {
            write_expr(out, g, d + 2);
        }
    }
    if let Some(h) = &s.having {
        line(out, d + 1, "Having");
        write_expr(out, h, d + 2);
    }
}

pub fn write_expr(out: &mut String, e: &Expr, d: usize) {
    match e {
        Expr::Identifier(i) => line(out, d, &format!("Column {}", i.value)),
        Expr::CompoundIdentifier(parts) => {
            let joined: Vec<&str> = parts.iter().map(|p| p.value.as_str()).collect();
            line(out, d, &format!("Column {}", joined.join(".")));
        }
        Expr::Literal { value, .. } => {
            let text = match value {
                Literal::Null => "NULL".to_string(),
                Literal::Boolean(b) => b.to_string().to_uppercase(),
                Literal::Number(n) => n.clone(),
                Literal::String(s) => format!("'{s}'"),
            };
            line(out, d, &format!("Literal {text}"));
        }
        Expr::BinaryOp { left, op, right, .. } => {
            line(out, d, &format!("BinaryOp {}", op.as_str()));
            write_expr(out, left, d + 1);
            write_expr(out, right, d + 1);
        }
        Expr::UnaryOp { op, expr, .. } => {
            line(out, d, &format!("UnaryOp {}", op.as_str()));
            write_expr(out, expr, d + 1);
        }
        Expr::Nested { expr, .. } => {
            line(out, d, "Nested");
            write_expr(out, expr, d + 1);
        }
        Expr::IsNull { expr, negated, .. } => {
            line(out, d, if *negated { "IsNotNull" } else { "IsNull" });
            write_expr(out, expr, d + 1);
        }
        Expr::Between { expr, negated, low, high, .. } => {
            line(out, d, if *negated { "NotBetween" } else { "Between" });
            write_expr(out, expr, d + 1);
            line(out, d + 1, "low");
            write_expr(out, low, d + 2);
            line(out, d + 1, "high");
            write_expr(out, high, d + 2);
        }
        Expr::InList { expr, list, negated, .. } => {
            line(out, d, if *negated { "NotInList" } else { "InList" });
            write_expr(out, expr, d + 1);
            line(out, d + 1, "list");
            for item in list {
                write_expr(out, item, d + 2);
            }
        }
        Expr::Like { expr, pattern, negated, case_insensitive, escape, .. } => {
            let mut label = String::new();
            if *negated {
                label.push_str("Not");
            }
            label.push_str(if *case_insensitive { "ILike" } else { "Like" });
            line(out, d, &label);
            write_expr(out, expr, d + 1);
            line(out, d + 1, "pattern");
            write_expr(out, pattern, d + 2);
            if let Some(esc) = escape {
                line(out, d + 1, "escape");
                write_expr(out, esc, d + 2);
            }
        }
        Expr::Cast { expr, data_type, .. } => {
            line(out, d, &format!("Cast to {data_type}"));
            write_expr(out, expr, d + 1);
        }
        Expr::Case { operand, when_then, else_result, .. } => {
            line(out, d, "Case");
            if let Some(op) = operand {
                line(out, d + 1, "operand");
                write_expr(out, op, d + 2);
            }
            for (w, t) in when_then {
                line(out, d + 1, "when");
                write_expr(out, w, d + 2);
                line(out, d + 1, "then");
                write_expr(out, t, d + 2);
            }
            if let Some(e) = else_result {
                line(out, d + 1, "else");
                write_expr(out, e, d + 2);
            }
        }
        Expr::ScalarSubquery { query, .. } => {
            line(out, d, "ScalarSubquery");
            write_query(out, query, d + 1);
        }
        Expr::Exists { query, negated, .. } => {
            line(out, d, if *negated { "NotExists" } else { "Exists" });
            write_query(out, query, d + 1);
        }
        Expr::InSubquery { expr, query, negated, .. } => {
            line(out, d, if *negated { "NotInSubquery" } else { "InSubquery" });
            write_expr(out, expr, d + 1);
            write_query(out, query, d + 1);
        }
        Expr::Function { name, args, distinct, over, .. } => {
            let mut header = String::new();
            let _ = write!(header, "Function {}", name.value);
            if *distinct {
                header.push_str(" distinct");
            }
            if over.is_some() {
                header.push_str(" over");
            }
            line(out, d, &header);
            for a in args {
                match a {
                    FunctionArg::Wildcard(_) => line(out, d + 1, "*"),
                    FunctionArg::Expr(e) => write_expr(out, e, d + 1),
                }
            }
            if let Some(spec) = over {
                if !spec.partition_by.is_empty() {
                    line(out, d + 1, "partition by");
                    for e in &spec.partition_by {
                        write_expr(out, e, d + 2);
                    }
                }
                if !spec.order_by.is_empty() {
                    line(out, d + 1, "order by");
                    for o in &spec.order_by {
                        write_expr(out, &o.expr, d + 2);
                    }
                }
                if let Some(frame) = &spec.frame {
                    line(
                        out,
                        d + 1,
                        &format!(
                            "frame {} {} to {}",
                            match frame.units {
                                FrameUnits::Rows => "rows",
                                FrameUnits::Range => "range",
                            },
                            bound_text(&frame.start),
                            bound_text(&frame.end)
                        ),
                    );
                }
            }
        }
    }
}
