//! The binder: name resolution, type checking, and the production of a typed
//! logical plan.
//!
//! This is the stage most toy engines skip, and it is where the semantics live.
//! After binding:
//!   * every table reference points at a real relation with a `RelId`;
//!   * every column reference is a `(RelId, index)` pair, with ambiguity
//!     already reported;
//!   * every expression has a resolved `DataType` and a nullability flag;
//!   * every implicit coercion is an explicit `Cast` node, so nothing
//!     downstream has to re-derive a type or guess at a conversion.
//!
//! Clauses are bound in SQL's *semantic* order (FROM, WHERE, then SELECT), not
//! the order they are written. That is why `SELECT a AS x FROM t WHERE x > 1`
//! correctly fails: `x` does not exist yet when WHERE is bound.

use std::sync::Arc;

use crate::catalog::Catalog;
use crate::error::{closest_match, Diagnostic, Result, Span};
use crate::parser::ast::{Cte, BinaryOperator, Expr, FromItem, FunctionArg, Ident, JoinConstraint, Literal, OrderByExpr,
    Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableRef, UnaryOperator,};
use crate::plan::{
    AggregateFunction, BoundAggregate, BoundExpr, BoundExprKind, BoundWindowFunction, ExprId,
    Frame, FrameBound, FrameUnits, JoinType, LogicalPlan, RelId, SortKey, SubqueryKind,
    WindowFunction,
};
use crate::storage::schema::Resolution;
use crate::storage::{Field, Schema};
use crate::types::{self, DataType, ScalarValue};

/// A relation visible to column resolution.
#[derive(Debug, Clone)]
struct ScopeEntry {
    rel: RelId,
    /// The name a qualifier must match: the alias if there is one, else the
    /// table name.
    binding: String,
    schema: Arc<Schema>,
}

/// State for binding the SELECT list and HAVING of an aggregate query.
///
/// Once a query aggregates, its output rows are no longer the input's rows, so
/// the only expressions that mean anything are the grouping keys and the
/// aggregates themselves. Everything bound while this is present is rewritten
/// into a reference to one of those, and a bare column that is neither is the
/// classic "column must appear in the GROUP BY clause" error.
struct AggregateScope {
    /// The `Aggregate` node's relation; the SELECT list reads its output.
    rel: RelId,
    /// Grouping expressions as written, for syntactic matching of compound
    /// keys like `GROUP BY a + 1`.
    group_asts: Vec<Expr>,
    group_exprs: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
}

pub struct Binder<'a> {
    catalog: &'a Catalog,
    next_rel: u32,
    next_expr: u32,
    /// One frame per query level, innermost last. Resolution searches the
    /// innermost frame first and walks outward; finding a column in an outer
    /// frame is what makes a subquery correlated.
    scopes: Vec<Vec<ScopeEntry>>,
    agg: Option<AggregateScope>,
    /// True while binding the argument of an aggregate, where a bare column
    /// reference is exactly what is wanted.
    inside_aggregate: bool,
    /// Window functions collected while binding the SELECT list, grouped by the
    /// `OVER (...)` clause they share.
    windows: Vec<WindowGroup>,
    /// One frame per `WITH` clause, innermost last. A name is looked up here
    /// before the catalog, so a CTE shadows a real table of the same name --
    /// which is what the standard says and what every engine does.
    ctes: Vec<Vec<CteBinding>>,
}

/// A bound `WITH` definition.
#[derive(Debug, Clone)]
struct CteBinding {
    /// Case-folded, because an unquoted reference matches case-insensitively.
    name: String,
    /// Shared with every reference, which is how the executor knows two
    /// references are to the same thing and computes it once.
    definition: Arc<LogicalPlan>,
    schema: Arc<Schema>,
}

/// One `OVER (...)` clause and the functions using it.
///
/// Functions with different windows need different sorts, so each group becomes
/// its own `Window` node and they stack. Grouping is by how the clause is
/// *written*, which is the same syntactic rule that matches a GROUP BY key.
struct WindowGroup {
    rel: RelId,
    spec: crate::parser::ast::WindowSpec,
    partition_by: Vec<BoundExpr>,
    order_by: Vec<SortKey>,
    functions: Vec<BoundWindowFunction>,
}

/// Bind a parsed statement against a catalog.
pub fn bind(catalog: &Catalog, stmt: &Statement) -> Result<LogicalPlan> {
    let mut binder = Binder::new(catalog);
    match stmt {
        Statement::Query(q) => binder.bind_query(q),
    }
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog) -> Binder<'a> {
        Binder {
            catalog,
            next_rel: 0,
            next_expr: 0,
            scopes: vec![Vec::new()],
            agg: None,
            inside_aggregate: false,
            windows: Vec::new(),
            ctes: Vec::new(),
        }
    }

    fn new_rel(&mut self) -> RelId {
        self.next_rel += 1;
        RelId(self.next_rel - 1)
    }

    fn new_expr_id(&mut self) -> ExprId {
        self.next_expr += 1;
        ExprId(self.next_expr - 1)
    }

    /// The innermost scope frame.
    fn scope(&mut self) -> &mut Vec<ScopeEntry> {
        self.scopes.last_mut().expect("there is always one frame")
    }

    fn expr(&mut self, kind: BoundExprKind, data_type: DataType, nullable: bool, span: Span) -> BoundExpr {
        BoundExpr {
            id: self.new_expr_id(),
            kind,
            data_type,
            nullable,
            span,
        }
    }

    // -- statements ---------------------------------------------------------

    pub fn bind_query(&mut self, q: &Query) -> Result<LogicalPlan> {
        // `WITH` definitions come into scope in the order written -- a later
        // one may name an earlier one -- and go out of scope with the query
        // that introduced them. The frame is popped whether binding the body
        // succeeded or not, or a failed subquery would leave its names visible.
        let scoped = !q.with.is_empty();
        if scoped {
            self.ctes.push(Vec::new());
            for cte in &q.with {
                if let Err(e) = self.bind_cte(cte) {
                    self.ctes.pop();
                    return Err(e);
                }
            }
        }
        let result = self.bind_query_body(q);
        if scoped {
            self.ctes.pop();
        }
        result
    }

    fn bind_query_body(&mut self, q: &Query) -> Result<LogicalPlan> {
        let mut plan = self.bind_set_expr(&q.body)?;

        // ORDER BY is applied to the query's *output*, after DISTINCT and after
        // any set operation, so the sort sits above everything and its keys are
        // expressed over the projected columns.
        if !q.order_by.is_empty() {
            plan = self.bind_order_by(plan, &q.order_by, q.as_select(), q.span)?;
        }

        // LIMIT / OFFSET. Both must be constants; a non-constant limit needs
        // an execution-time parameter, which is a separate feature.
        let skip = match &q.offset {
            Some(e) => self.bind_count(e, "OFFSET")?,
            None => 0,
        };
        let fetch = match &q.limit {
            Some(e) => Some(self.bind_count(e, "LIMIT")?),
            None => None,
        };
        if fetch.is_some() || skip > 0 {
            plan = LogicalPlan::Limit {
                rel: self.new_rel(),
                skip,
                fetch,
                input: Box::new(plan),
            };
        }
        Ok(plan)
    }

    fn bind_set_expr(&mut self, body: &SetExpr) -> Result<LogicalPlan> {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::SetOp { op, all, left, right, span } => {
                // Each branch gets its own scope: the two sides of a UNION are
                // separate queries that happen to be combined, and neither can
                // see the other's tables.
                let left = self.bind_branch(left)?;
                let right = self.bind_branch(right)?;
                self.bind_set_op(*op, *all, left, right, *span)
            }
        }
    }

    fn bind_branch(&mut self, body: &SetExpr) -> Result<LogicalPlan> {
        let saved = std::mem::replace(&mut self.scopes, vec![Vec::new()]);
        let result = self.bind_set_expr(body);
        self.scopes = saved;
        result
    }

    /// Combine two branches, coercing their columns to a common type.
    ///
    /// The two sides must agree on arity; where they disagree on type, both are
    /// widened to whatever both can become. Without that,
    /// `SELECT 1 UNION SELECT 2.5` would have no single output type.
    fn bind_set_op(
        &mut self,
        op: crate::parser::ast::SetOperator,
        all: bool,
        left: LogicalPlan,
        right: LogicalPlan,
        span: Span,
    ) -> Result<LogicalPlan> {
        let (ls, rs) = (left.schema(), right.schema());
        if ls.len() != rs.len() {
            return Err(Diagnostic::bind(
                format!(
                    "the two sides of a {} must have the same number of columns, found {} and {}",
                    op.as_str(),
                    ls.len(),
                    rs.len()
                ),
                span,
            ));
        }

        let mut fields = Vec::with_capacity(ls.len());
        for (l, r) in ls.fields.iter().zip(&rs.fields) {
            let Some(common) = types::common_type(l.data_type, r.data_type) else {
                return Err(Diagnostic::bind(
                    format!(
                        "column `{}` is {} on one side of the {} and {} on the other",
                        l.name,
                        l.data_type,
                        op.as_str(),
                        r.data_type
                    ),
                    span,
                ));
            };
            // The output takes its names from the first branch, as SQL says.
            fields.push(Field::new(
                l.name.clone(),
                common,
                l.nullable || r.nullable,
            ));
        }
        let schema = Arc::new(Schema::new(fields));

        let left = self.cast_branch(left, &schema, span)?;
        let right = self.cast_branch(right, &schema, span)?;

        let plan = LogicalPlan::SetOp {
            rel: self.new_rel(),
            op: match op {
                crate::parser::ast::SetOperator::Union => crate::plan::SetOperator::Union,
                crate::parser::ast::SetOperator::Intersect => crate::plan::SetOperator::Intersect,
                crate::parser::ast::SetOperator::Except => crate::plan::SetOperator::Except,
            },
            all,
            left: Box::new(left),
            right: Box::new(right),
            schema,
        };
        Ok(plan)
    }

    /// Project a branch onto the set operation's column types, if it is not
    /// already there.
    fn cast_branch(
        &mut self,
        plan: LogicalPlan,
        target: &Arc<Schema>,
        span: Span,
    ) -> Result<LogicalPlan> {
        let schema = plan.schema();
        if schema
            .fields
            .iter()
            .zip(&target.fields)
            .all(|(a, b)| a.data_type == b.data_type)
        {
            return Ok(plan);
        }
        let rel = plan.rel();
        let mut exprs = Vec::with_capacity(target.len());
        for (index, field) in target.fields.iter().enumerate() {
            let source = schema.field(index);
            let column = self.expr(
                BoundExprKind::Column {
                    rel,
                    index,
                    name: source.name.clone(),
                },
                source.data_type,
                source.nullable,
                span,
            );
            exprs.push(self.coerce_to(column, field.data_type, span)?);
        }
        Ok(LogicalPlan::Project {
            rel: self.new_rel(),
            exprs,
            schema: Arc::clone(target),
            input: Box::new(plan),
        })
    }

    fn bind_select(&mut self, select: &Select) -> Result<LogicalPlan> {
        // 1. FROM -- establishes the scope everything else resolves against.
        let mut plan = self.bind_from(&select.from)?;

        // 2. WHERE. Bound before SELECT, so output aliases are deliberately
        //    not visible here -- and before GROUP BY, so an aggregate in WHERE
        //    is an error rather than a filter on the aggregated result.
        if let Some(pred) = &select.selection {
            if expr_has_aggregate(pred) {
                return Err(Diagnostic::bind(
                    "aggregate functions are not allowed in WHERE",
                    pred.span(),
                )
                .with_hint("filter aggregated rows with HAVING instead"));
            }
            let predicate = self.bind_expr(pred)?;
            let predicate = self.require_boolean(predicate, "WHERE")?;
            plan = LogicalPlan::Filter {
                rel: self.new_rel(),
                predicate,
                input: Box::new(plan),
            };
        }

        // 3. Is this an aggregate query? Either it groups, or an aggregate call
        //    appears somewhere that makes the whole query collapse to one row.
        let aggregating = !select.group_by.is_empty()
            || select.having.is_some()
            || select
                .projection
                .iter()
                .any(|item| matches!(item, SelectItem::Expr { expr, .. } if expr_has_aggregate(expr)));

        if aggregating {
            return self.bind_aggregate_select(select, plan);
        }

        // 4. SELECT list, with `*` expanded to explicit columns. Window
        //    functions found here are collected rather than bound in place; the
        //    Window nodes that compute them go in below the projection.
        let saved_windows = std::mem::take(&mut self.windows);
        let mut exprs = Vec::new();
        let mut fields = Vec::new();
        let result = (|| {
            for item in &select.projection {
                self.bind_select_item(item, &mut exprs, &mut fields)?;
            }
            Ok(())
        })();
        let groups = std::mem::replace(&mut self.windows, saved_windows);
        result?;

        let plan = self.apply_windows(plan, groups);

        let rel = self.new_rel();
        let plan = LogicalPlan::Project {
            rel,
            exprs,
            schema: Arc::new(Schema::new(fields)),
            input: Box::new(plan),
        };
        Ok(self.maybe_distinct(plan, select.distinct))
    }

    /// Stack one `Window` node per distinct `OVER (...)` clause.
    ///
    /// A window function's column is addressed as
    /// `(group relation, position within the group)` -- *relative* to the
    /// window's own columns, not an absolute position in its output. That
    /// matters: the absolute position depends on how wide the input is, and
    /// projection pushdown changes exactly that. The relative form is resolved
    /// at execution time against whatever the input turned out to be.
    fn apply_windows(&mut self, mut plan: LogicalPlan, groups: Vec<WindowGroup>) -> LogicalPlan {
        for group in groups {
            let schema = crate::plan::window_schema(&group.functions, &plan);
            plan = LogicalPlan::Window {
                rel: group.rel,
                partition_by: group.partition_by,
                order_by: group.order_by,
                functions: group.functions,
                schema,
                input: Box::new(plan),
            };
        }
        plan
    }

    /// Bind a `func(...) OVER (...)` call, returning a reference to the column
    /// the window node will produce.
    fn bind_window_function(
        &mut self,
        name: &Ident,
        args: &[FunctionArg],
        distinct: bool,
        spec: &crate::parser::ast::WindowSpec,
        span: Span,
    ) -> Result<BoundExpr> {
        let Some(func) = WindowFunction::from_name(&name.value) else {
            return Err(Diagnostic::bind(
                format!("no window function named `{}`", name.value),
                span,
            ));
        };
        if self.agg.is_some() {
            // A window runs *after* aggregation, so its arguments would have to
            // be resolved against the aggregate's output. Getting that wrong
            // gives a wrong answer rather than an error, so it is refused.
            return Err(Diagnostic::bind(
                "window functions in an aggregate query are not supported yet",
                span,
            ));
        }
        if distinct {
            return Err(Diagnostic::bind(
                "DISTINCT is not allowed in a window function",
                span,
            ));
        }

        // Arguments read the window's *input*, so they bind in the ordinary
        // scope.
        let mut bound_args = Vec::new();
        for a in args {
            match a {
                FunctionArg::Wildcard(w) => {
                    if func != WindowFunction::Aggregate(AggregateFunction::Count) {
                        return Err(Diagnostic::bind(
                            format!("`{}` does not accept `*`", func.as_str()),
                            *w,
                        ));
                    }
                }
                FunctionArg::Expr(e) => bound_args.push(self.bind_expr(e)?),
            }
        }
        let (data_type, nullable) = window_result_type(func, &bound_args, span)?;

        // Find or create the group for this OVER clause.
        let index = match self.windows.iter().position(|g| window_spec_eq(&g.spec, spec)) {
            Some(i) => i,
            None => {
                let (partition_by, order_by) = self.bind_window_keys(spec)?;
                let rel = self.new_rel();
                self.windows.push(WindowGroup {
                    rel,
                    spec: spec.clone(),
                    partition_by,
                    order_by,
                    functions: Vec::new(),
                });
                self.windows.len() - 1
            }
        };

        let frame = resolve_frame(spec, func, span)?;
        let label = format!(
            "{}({})",
            func.as_str(),
            bound_args
                .iter()
                .map(|a| a.to_sql())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let group = &mut self.windows[index];
        let position = group.functions.len();
        group.functions.push(BoundWindowFunction {
            func,
            args: bound_args,
            frame,
            data_type,
            nullable,
            label: label.clone(),
        });
        let rel = group.rel;

        Ok(self.expr(
            BoundExprKind::Column {
                rel,
                // Position within the group for now; `apply_windows` turns it
                // into a real column index once every group's width is known.
                index: position,
                name: label,
            },
            data_type,
            nullable,
            span,
        ))
    }

    fn bind_window_keys(
        &mut self,
        spec: &crate::parser::ast::WindowSpec,
    ) -> Result<(Vec<BoundExpr>, Vec<SortKey>)> {
        let mut partition_by = Vec::with_capacity(spec.partition_by.len());
        for e in &spec.partition_by {
            partition_by.push(self.bind_expr(e)?);
        }
        let mut order_by = Vec::with_capacity(spec.order_by.len());
        for o in &spec.order_by {
            let expr = self.bind_expr(&o.expr)?;
            let ascending = o.asc.unwrap_or(true);
            order_by.push(SortKey {
                expr,
                ascending,
                nulls_first: o.nulls_first.unwrap_or(ascending),
            });
        }
        Ok((partition_by, order_by))
    }

    /// DISTINCT applies to the projected rows, so it goes above the projection
    /// and below any ORDER BY.
    fn maybe_distinct(&mut self, plan: LogicalPlan, distinct: bool) -> LogicalPlan {
        if !distinct {
            return plan;
        }
        LogicalPlan::Distinct {
            rel: self.new_rel(),
            input: Box::new(plan),
        }
    }

    /// Bind the SELECT list and HAVING of an aggregating query.
    ///
    /// Grouping keys are bound first, in the ordinary scope. Everything after
    /// that is bound with `self.agg` set, which rewrites each grouping key and
    /// each aggregate call into a reference to the `Aggregate` node's output --
    /// and rejects anything else that reads a base column.
    fn bind_aggregate_select(
        &mut self,
        select: &Select,
        input: LogicalPlan,
    ) -> Result<LogicalPlan> {
        let mut group_exprs = Vec::with_capacity(select.group_by.len());
        for g in &select.group_by {
            if expr_has_aggregate(g) {
                return Err(Diagnostic::bind(
                    "aggregate functions are not allowed in GROUP BY",
                    g.span(),
                ));
            }
            group_exprs.push(self.bind_expr(g)?);
        }

        let agg_rel = self.new_rel();
        self.agg = Some(AggregateScope {
            rel: agg_rel,
            group_asts: select.group_by.clone(),
            group_exprs,
            aggregates: Vec::new(),
        });

        // HAVING is bound before the SELECT list because it is applied first,
        // and because both contribute aggregates to the same node.
        let having = match &select.having {
            Some(h) => {
                let bound = self.bind_expr(h)?;
                Some(self.require_boolean(bound, "HAVING")?)
            }
            None => None,
        };

        let mut exprs = Vec::new();
        let mut fields = Vec::new();
        for item in &select.projection {
            self.bind_select_item(item, &mut exprs, &mut fields)?;
        }

        let scope = self.agg.take().expect("aggregate scope was installed above");

        // The aggregate node's own output: grouping keys, then aggregates.
        let mut agg_fields = Vec::with_capacity(scope.group_exprs.len() + scope.aggregates.len());
        for (e, ast) in scope.group_exprs.iter().zip(&scope.group_asts) {
            agg_fields.push(Field::new(
                match &e.kind {
                    BoundExprKind::Column { name, .. } => name.clone(),
                    _ => strip_outer_parens(&e.to_sql()),
                },
                e.data_type,
                e.nullable,
            ));
            let _ = ast;
        }
        for a in &scope.aggregates {
            agg_fields.push(Field::new(a.to_sql(), a.data_type, a.nullable));
        }

        let mut plan = LogicalPlan::Aggregate {
            rel: agg_rel,
            group_exprs: scope.group_exprs,
            aggregates: scope.aggregates,
            schema: Arc::new(Schema::new(agg_fields)),
            input: Box::new(input),
        };

        if let Some(predicate) = having {
            plan = LogicalPlan::Filter {
                rel: self.new_rel(),
                predicate,
                input: Box::new(plan),
            };
        }

        let projected = LogicalPlan::Project {
            rel: self.new_rel(),
            exprs,
            schema: Arc::new(Schema::new(fields)),
            input: Box::new(plan),
        };
        Ok(self.maybe_distinct(projected, select.distinct))
    }

    /// Resolve ORDER BY against the query's output.
    ///
    /// A sort key can be an output column -- named, or by 1-based position --
    /// or an expression over the input that was never selected. The second kind
    /// has nowhere to be evaluated above the projection, so it is appended as a
    /// hidden column and trimmed off again after the sort. That is the standard
    /// way to make `SELECT a FROM t ORDER BY b` work.
    fn bind_order_by(
        &mut self,
        plan: LogicalPlan,
        order_by: &[OrderByExpr],
        select: Option<&Select>,
        span: Span,
    ) -> Result<LogicalPlan> {
        // A set operation's output is not a projection, so it cannot grow a
        // hidden column -- sort it by wrapping it in one.
        let plan = match plan {
            set @ LogicalPlan::SetOp { .. } => self.project_all(set, span),
            other => other,
        };
        let (project, distinct_rel) = match plan {
            LogicalPlan::Distinct { rel, input } => (*input, Some(rel)),
            other => (other, None),
        };
        let LogicalPlan::Project { rel, mut exprs, schema, input } = project else {
            return Err(Diagnostic::bind("ORDER BY is not supported here", span));
        };

        let visible = schema.len();
        let mut fields = schema.fields.clone();
        let mut keys = Vec::with_capacity(order_by.len());

        for item in order_by {
            let index = match self.resolve_order_key(&item.expr, select, &fields[..visible])? {
                Some(index) => index,
                None => {
                    // Not one of the output columns. It has to be computed, so
                    // it becomes a hidden column of the projection.
                    if distinct_rel.is_some() {
                        return Err(Diagnostic::bind(
                            "ORDER BY must name an output column of a SELECT DISTINCT",
                            item.expr.span(),
                        )
                        .with_hint("sorting on something not selected would make \
                                    which duplicate survives arbitrary"));
                    }
                    if select.is_none_or(|s| self.aggregating(s)) {
                        return Err(Diagnostic::bind(
                            "ORDER BY must name an output column or position here",
                            item.expr.span(),
                        ));
                    }
                    let bound = self.bind_expr(&item.expr)?;
                    fields.push(Field::new(
                        format!("order_by_{}", fields.len()),
                        bound.data_type,
                        bound.nullable,
                    ));
                    exprs.push(bound);
                    fields.len() - 1
                }
            };

            let field = &fields[index];
            let ascending = item.asc.unwrap_or(true);
            let key_expr = self.expr(
                BoundExprKind::Column {
                    rel,
                    index,
                    name: field.name.clone(),
                },
                field.data_type,
                field.nullable,
                item.expr.span(),
            );
            keys.push(SortKey {
                expr: key_expr,
                ascending,
                // SQLite's default, which is what the corpus compares against.
                nulls_first: item.nulls_first.unwrap_or(ascending),
            });
        }

        let hidden = fields.len() - visible;
        let mut plan = LogicalPlan::Project {
            rel,
            exprs,
            schema: Arc::new(Schema::new(fields.clone())),
            input,
        };
        if let Some(distinct) = distinct_rel {
            plan = LogicalPlan::Distinct {
                rel: distinct,
                input: Box::new(plan),
            };
        }
        plan = LogicalPlan::Sort {
            rel: self.new_rel(),
            keys,
            input: Box::new(plan),
        };

        if hidden > 0 {
            // Trim the columns that only existed to be sorted on.
            let trim_rel = self.new_rel();
            let exprs = (0..visible)
                .map(|index| {
                    let field = fields[index].clone();
                    self.expr(
                        BoundExprKind::Column {
                            rel,
                            index,
                            name: field.name.clone(),
                        },
                        field.data_type,
                        field.nullable,
                        span,
                    )
                })
                .collect();
            plan = LogicalPlan::Project {
                rel: trim_rel,
                exprs,
                schema: Arc::new(Schema::new(fields[..visible].to_vec())),
                input: Box::new(plan),
            };
        }
        Ok(plan)
    }

    /// Identity projection, so a plan that is not a projection can still grow
    /// the hidden columns an ORDER BY needs.
    fn project_all(&mut self, plan: LogicalPlan, span: Span) -> LogicalPlan {
        let schema = plan.schema();
        let rel = plan.rel();
        let exprs = schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                self.expr(
                    BoundExprKind::Column {
                        rel,
                        index,
                        name: field.name.clone(),
                    },
                    field.data_type,
                    field.nullable,
                    span,
                )
            })
            .collect();
        LogicalPlan::Project {
            rel: self.new_rel(),
            exprs,
            schema,
            input: Box::new(plan),
        }
    }

    /// Which output column an ORDER BY item names, if any.
    fn resolve_order_key(
        &mut self,
        expr: &Expr,
        select: Option<&Select>,
        fields: &[Field],
    ) -> Result<Option<usize>> {
        // A bare integer is a 1-based output position.
        if let Expr::Literal { value: Literal::Number(text), span } = expr {
            if let Ok(position) = text.parse::<usize>() {
                if position == 0 || position > fields.len() {
                    return Err(Diagnostic::bind(
                        format!(
                            "ORDER BY position {position} is out of range, the query has {} columns",
                            fields.len()
                        ),
                        *span,
                    ));
                }
                return Ok(Some(position - 1));
            }
        }

        // An output column name, which includes any alias the query gave.
        if let Expr::Identifier(name) = expr {
            let wanted = name.normalized();
            if let Some(index) = fields.iter().position(|f| {
                if name.quoted {
                    f.name == wanted
                } else {
                    f.name.eq_ignore_ascii_case(&wanted)
                }
            }) {
                return Ok(Some(index));
            }
        }

        // Written the same way as one of the select items. This is what makes
        // `ORDER BY COUNT(*)` work: the aggregate has already been bound into
        // the projection, and binding it again here would register a second one.
        let position = select.and_then(|s| {
            s.projection.iter().position(|item| match item {
                SelectItem::Expr { expr: selected, .. } => ast_eq(selected, expr),
                _ => false,
            })
        });
        Ok(position.filter(|p| *p < fields.len()))
    }

    fn aggregating(&self, select: &Select) -> bool {
        !select.group_by.is_empty()
            || select.having.is_some()
            || select
                .projection
                .iter()
                .any(|item| matches!(item, SelectItem::Expr { expr, .. } if expr_has_aggregate(expr)))
    }

    fn bind_from(&mut self, from: &[FromItem]) -> Result<LogicalPlan> {
        let Some((first, rest)) = from.split_first() else {
            return Ok(LogicalPlan::OneRow { rel: self.new_rel() });
        };
        let mut plan = self.bind_from_item(first)?;
        for item in rest {
            // A comma between FROM items is a cross join; any condition
            // relating them lands in WHERE, which is where the optimizer will
            // later find it and turn the cross join into an inner one.
            let right = self.bind_from_item(item)?;
            plan = self.make_join(plan, right, JoinType::Cross, None);
        }
        Ok(plan)
    }

    fn bind_from_item(&mut self, item: &FromItem) -> Result<LogicalPlan> {
        let mut plan = self.bind_table_ref(&item.relation)?;
        for join in &item.joins {
            // The right relation is pushed into scope before the condition is
            // bound, which is what lets `ON a.id = b.id` see both sides.
            let right = self.bind_table_ref(&join.relation)?;
            let join_type = JoinType::from_ast(join.operator);

            let on = match &join.constraint {
                JoinConstraint::On(e) => {
                    if expr_has_aggregate(e) {
                        return Err(Diagnostic::bind(
                            "aggregate functions are not allowed in a join condition",
                            e.span(),
                        ));
                    }
                    let bound = self.bind_expr(e)?;
                    Some(self.require_boolean(bound, "JOIN ... ON")?)
                }
                JoinConstraint::Using(cols) => {
                    let span = cols
                        .first()
                        .map(|c| c.span.merge(cols.last().unwrap().span))
                        .unwrap_or(join.span);
                    return Err(Diagnostic::bind("USING is not supported yet", span).with_hint(
                        "write the equality out with ON; USING also merges the paired \
                         columns in `SELECT *`, which is not implemented",
                    ));
                }
                JoinConstraint::None => None,
            };
            plan = self.make_join(plan, right, join_type, on);
        }
        Ok(plan)
    }

    /// Assemble a join node, widening nullability on whichever side can be
    /// left unmatched.
    ///
    /// This is the fact every outer-join rewrite rule depends on: a LEFT join
    /// can emit a row whose right-hand columns are all NULL even when the base
    /// columns are declared NOT NULL. Recording that here is what will make
    /// "only push a predicate into the preserved side" checkable rather than a
    /// comment.
    fn make_join(
        &mut self,
        left: LogicalPlan,
        right: LogicalPlan,
        join_type: JoinType,
        on: Option<BoundExpr>,
    ) -> LogicalPlan {
        // Widen the *scope* too, not just this node's schema. A column
        // reference bound after this join -- in a later ON, in WHERE, in the
        // SELECT list -- must see that the padded side can now be NULL, or the
        // nullability flags every outer-join rewrite depends on will be wrong.
        if join_type.preserves_right() {
            self.widen_scope(&left);
        }
        if join_type.preserves_left() {
            self.widen_scope(&right);
        }

        let schema = crate::plan::join_schema(join_type, &left, &right);
        LogicalPlan::Join {
            rel: self.new_rel(),
            join_type,
            on,
            left: Box::new(left),
            right: Box::new(right),
            schema,
        }
    }

    fn bind_table_ref(&mut self, t: &TableRef) -> Result<LogicalPlan> {
        match &t.factor {
            TableFactor::Table(name) => self.bind_named_table(name, t.alias.as_ref()),
            TableFactor::Derived(query) => {
                let alias = t
                    .alias
                    .as_ref()
                    .expect("the parser requires a derived table to be aliased");
                self.bind_derived_table(query, alias)
            }
        }
    }

    fn bind_named_table(&mut self, name: &Ident, alias: Option<&Ident>) -> Result<LogicalPlan> {
        let wanted = name.normalized();

        // A CTE shadows a catalog table of the same name.
        if let Some(cte) = self.lookup_cte(&wanted) {
            let rel = self.new_rel();
            let binding = match alias {
                Some(a) => a.normalized(),
                None => cte.name.clone(),
            };
            self.scope().push(ScopeEntry {
                rel,
                binding,
                schema: Arc::clone(&cte.schema),
            });
            return Ok(LogicalPlan::CteRef {
                rel,
                name: cte.name.clone(),
                definition: cte.definition,
                schema: cte.schema,
            });
        }

        let Some(table) = self.catalog.get(&wanted, name.quoted) else {
            let names = self.catalog.table_names();
            let mut d = Diagnostic::bind(format!("no such table `{}`", name.value), name.span);
            if names.is_empty() {
                d = d.with_hint("no tables are loaded; try `.load <name> <file.csv>`");
            } else if let Some(sug) = closest_match(&wanted, names.iter().map(|s| s.as_str())) {
                d = d.with_hint(format!("did you mean `{sug}`?"));
            }
            return Err(d);
        };

        let rel = self.new_rel();
        // The qualifier is the alias if one was given, otherwise the table
        // name. Once aliased, the original table name is no longer usable as a
        // qualifier -- that is standard SQL and it is what makes self-joins work.
        let binding = match alias {
            Some(a) => a.normalized(),
            None => table.name.clone(),
        };
        self.scope().push(ScopeEntry {
            rel,
            binding,
            schema: Arc::clone(&table.schema),
        });

        Ok(LogicalPlan::Scan {
            rel,
            table_name: table.name.clone(),
            table_schema: Arc::clone(&table.schema),
            projection: None,
            schema: Arc::clone(&table.schema),
        })
    }

    /// `FROM (SELECT ...) alias`.
    ///
    /// The subquery is bound in a fresh scope frame -- a derived table cannot
    /// see the query it appears in, so a correlated reference here would be an
    /// error rather than a correlation. The result is wrapped in a
    /// `SubqueryAlias`, which gives the projection's anonymous columns a
    /// relation to belong to so `alias.col` resolves.
    fn bind_derived_table(&mut self, query: &Query, alias: &Ident) -> Result<LogicalPlan> {
        let inner = self.bind_isolated_query(query)?;
        let schema = inner.schema();
        let rel = self.new_rel();
        self.scope().push(ScopeEntry {
            rel,
            binding: alias.normalized(),
            schema: Arc::clone(&schema),
        });
        Ok(LogicalPlan::SubqueryAlias {
            rel,
            alias: alias.value.clone(),
            schema,
            input: Box::new(inner),
        })
    }

    /// Bind a query that cannot see the enclosing one at all.
    /// Bind one `WITH` definition and put its name in scope.
    ///
    /// A CTE cannot see the query that introduces it -- it is a standalone
    /// query with a name -- so it binds in an isolated scope, exactly as a
    /// derived table does.
    fn bind_cte(&mut self, cte: &Cte) -> Result<()> {
        let name = cte.name.normalized();
        if self
            .ctes
            .last()
            .is_some_and(|frame| frame.iter().any(|b| b.name == name))
        {
            return Err(Diagnostic::bind(
                format!("`{}` is defined twice in the same WITH clause", cte.name.value),
                cte.name.span,
            ));
        }

        let plan = self.bind_isolated_query(&cte.query)?;
        let inner = plan.schema();

        // `WITH t(a, b) AS (...)` renames the output columns positionally.
        let schema = if cte.columns.is_empty() {
            inner
        } else {
            if cte.columns.len() != inner.len() {
                return Err(Diagnostic::bind(
                    format!(
                        "`{}` names {} column(s) but its query produces {}",
                        cte.name.value,
                        cte.columns.len(),
                        inner.len()
                    ),
                    cte.span,
                ));
            }
            Arc::new(Schema::new(
                inner
                    .fields
                    .iter()
                    .zip(&cte.columns)
                    .map(|(f, name)| Field::new(name.normalized(), f.data_type, f.nullable))
                    .collect(),
            ))
        };

        self.ctes.last_mut().expect("a frame was pushed").push(CteBinding {
            name,
            definition: Arc::new(plan),
            schema,
        });
        Ok(())
    }

    /// The innermost `WITH` binding for a name, if any.
    fn lookup_cte(&self, name: &str) -> Option<CteBinding> {
        self.ctes
            .iter()
            .rev()
            .find_map(|frame| frame.iter().find(|b| b.name == name))
            .cloned()
    }

    fn bind_isolated_query(&mut self, query: &Query) -> Result<LogicalPlan> {
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![Vec::new()]);
        let saved_agg = self.agg.take();
        let saved_inside = std::mem::replace(&mut self.inside_aggregate, false);
        let result = self.bind_query(query);
        self.scopes = saved_scopes;
        self.agg = saved_agg;
        self.inside_aggregate = saved_inside;
        result
    }

    /// Bind a subquery that *can* see the enclosing query, which is what makes
    /// a correlated reference possible.
    fn bind_nested_query(&mut self, query: &Query, span: Span) -> Result<LogicalPlan> {
        if self.agg.is_some() {
            // A correlated reference from here would have to resolve against
            // the aggregate's output rather than its input, and getting that
            // wrong produces a wrong answer rather than an error.
            return Err(Diagnostic::bind(
                "subqueries in the SELECT list or HAVING of an aggregate query are not supported yet",
                span,
            ));
        }
        self.scopes.push(Vec::new());
        let saved_agg = self.agg.take();
        let saved_inside = std::mem::replace(&mut self.inside_aggregate, false);
        let result = self.bind_query(query);
        self.scopes.pop();
        self.agg = saved_agg;
        self.inside_aggregate = saved_inside;
        result
    }

    /// Mark every relation in `plan` as producing nullable columns, because a
    /// join above it can emit rows where they are NULL-padded.
    fn widen_scope(&mut self, plan: &LogicalPlan) {
        let mut rels = Vec::new();
        collect_relations(plan, &mut rels);
        for entry in self.scopes.iter_mut().flatten() {
            if !rels.contains(&entry.rel) {
                continue;
            }
            let widened: Vec<Field> = entry
                .schema
                .fields
                .iter()
                .map(|f| Field::new(f.name.clone(), f.data_type, true))
                .collect();
            entry.schema = Arc::new(Schema::new(widened));
        }
    }

    fn bind_select_item(
        &mut self,
        item: &SelectItem,
        exprs: &mut Vec<BoundExpr>,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        match item {
            SelectItem::Wildcard { span } => {
                if self.visible_scope().is_empty() {
                    return Err(Diagnostic::bind("`*` requires a FROM clause", *span));
                }
                let entries = self.visible_scope();
                for entry in entries {
                    self.expand_relation(&entry, *span, exprs, fields);
                }
                Ok(())
            }
            SelectItem::QualifiedWildcard { qualifier, span } => {
                let entry = self.lookup_relation(qualifier)?;
                self.expand_relation(&entry, *span, exprs, fields);
                Ok(())
            }
            SelectItem::Expr { expr, alias } => {
                let bound = self.bind_expr(expr)?;
                let name = match alias {
                    Some(a) => a.value.clone(),
                    // Unaliased output columns are named after the column they
                    // came from, or after the expression text otherwise.
                    None => match &bound.kind {
                        BoundExprKind::Column { name, .. } => name.clone(),
                        _ => strip_outer_parens(&bound.to_sql()),
                    },
                };
                fields.push(Field::new(name, bound.data_type, bound.nullable));
                exprs.push(bound);
                Ok(())
            }
        }
    }

    fn expand_relation(
        &mut self,
        entry: &ScopeEntry,
        span: Span,
        exprs: &mut Vec<BoundExpr>,
        fields: &mut Vec<Field>,
    ) {
        for (index, f) in entry.schema.fields.iter().enumerate() {
            let kind = BoundExprKind::Column {
                rel: entry.rel,
                index,
                name: f.name.clone(),
            };
            let e = self.expr(kind, f.data_type, f.nullable, span);
            fields.push(f.clone());
            exprs.push(e);
        }
    }

    /// LIMIT / OFFSET values: constant, integral and non-negative.
    fn bind_count(&mut self, e: &Expr, clause: &str) -> Result<usize> {
        let bound = self.bind_expr(e)?;
        let span = bound.span;
        let BoundExprKind::Literal(v) = &bound.kind else {
            return Err(Diagnostic::bind(
                format!("{clause} must be a constant"),
                span,
            ));
        };
        let n = match v {
            ScalarValue::Int32(i) => *i as i64,
            ScalarValue::Int64(i) => *i,
            _ => {
                return Err(Diagnostic::bind(
                    format!("{clause} must be an integer, found {}", bound.data_type),
                    span,
                ))
            }
        };
        if n < 0 {
            return Err(Diagnostic::bind(
                format!("{clause} must not be negative"),
                span,
            ));
        }
        Ok(n as usize)
    }

    // -- expressions --------------------------------------------------------

    pub fn bind_expr(&mut self, e: &Expr) -> Result<BoundExpr> {
        // In an aggregate query, an expression that *is* one of the grouping
        // keys resolves to that key's slot in the aggregate's output rather
        // than being rebuilt from base columns. Matching is syntactic, which is
        // what makes `SELECT a + 1 ... GROUP BY a + 1` work; plain column
        // references are matched more robustly in `resolve_column`.
        if self.agg.is_some() && !self.inside_aggregate {
            if let Some(index) = self.match_group_expr(e) {
                return Ok(self.group_slot(index, e.span()));
            }
        }
        match e {
            // Parentheses carry no semantics past parsing.
            Expr::Nested { expr, .. } => self.bind_expr(expr),

            Expr::Identifier(name) => self.resolve_column(None, name),
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [qualifier, name] => self.resolve_column(Some(qualifier), name),
                _ => Err(Diagnostic::bind(
                    "qualified names may have at most two parts (`table.column`)",
                    e.span(),
                )),
            },

            Expr::Literal { value, span } => self.bind_literal(value, *span),
            Expr::BinaryOp { left, op, right, op_span } => {
                self.bind_binary(left, *op, right, *op_span, e.span())
            }
            Expr::UnaryOp { op, expr, span } => self.bind_unary(*op, expr, *span),
            Expr::Cast { expr, data_type, span } => {
                let inner = self.bind_expr(expr)?;
                let nullable = inner.nullable;
                Ok(self.expr(
                    BoundExprKind::Cast { expr: Box::new(inner), implicit: false },
                    *data_type,
                    nullable,
                    *span,
                ))
            }
            Expr::IsNull { expr, negated, span } => {
                let inner = self.bind_expr(expr)?;
                // IS NULL is the one comparison that never returns NULL: it
                // answers a question *about* nullness rather than propagating it.
                Ok(self.expr(
                    BoundExprKind::IsNull { expr: Box::new(inner), negated: *negated },
                    DataType::Boolean,
                    false,
                    *span,
                ))
            }
            Expr::Between { expr, negated, low, high, span } => {
                self.bind_between(expr, *negated, low, high, *span)
            }
            Expr::InList { expr, list, negated, span } => {
                self.bind_in_list(expr, list, *negated, *span)
            }
            Expr::Like { expr, pattern, negated, case_insensitive, escape, span } => {
                self.bind_like(expr, pattern, *negated, *case_insensitive, escape.as_deref(), *span)
            }
            Expr::Case { operand, when_then, else_result, span } => {
                self.bind_case(operand.as_deref(), when_then, else_result.as_deref(), *span)
            }
            Expr::Function { name, args, distinct, over, span } => match over {
                Some(spec) => self.bind_window_function(name, args, *distinct, spec, *span),
                None => self.bind_function(name, args, *distinct, *span),
            },

            Expr::ScalarSubquery { query, span } => {
                let plan = self.bind_nested_query(query, *span)?;
                let schema = plan.schema();
                if schema.len() != 1 {
                    return Err(Diagnostic::bind(
                        format!(
                            "a subquery used as a value must return one column, this one returns {}",
                            schema.len()
                        ),
                        *span,
                    ));
                }
                let field = schema.field(0).clone();
                Ok(self.expr(
                    BoundExprKind::Subquery {
                        kind: SubqueryKind::Scalar,
                        plan: Arc::new(plan),
                    },
                    field.data_type,
                    // Always nullable: a subquery returning no rows is NULL,
                    // whatever the column's own nullability says.
                    true,
                    *span,
                ))
            }

            Expr::Exists { query, negated, span } => {
                let plan = self.bind_nested_query(query, *span)?;
                Ok(self.expr(
                    BoundExprKind::Subquery {
                        kind: SubqueryKind::Exists { negated: *negated },
                        plan: Arc::new(plan),
                    },
                    DataType::Boolean,
                    // EXISTS asks whether any row came back, which is never
                    // unknown however NULL its contents are.
                    false,
                    *span,
                ))
            }

            Expr::InSubquery { expr, query, negated, span } => {
                let value = self.bind_expr(expr)?;
                let plan = self.bind_nested_query(query, *span)?;
                let schema = plan.schema();
                if schema.len() != 1 {
                    return Err(Diagnostic::bind(
                        format!(
                            "the subquery of an IN must return one column, this one returns {}",
                            schema.len()
                        ),
                        *span,
                    ));
                }
                let column_type = schema.field(0).data_type;
                let value = match types::common_type(value.data_type, column_type) {
                    Some(common) => self.coerce_to(value, common, *span)?,
                    None => {
                        return Err(Diagnostic::bind(
                            format!(
                                "IN cannot compare {} with the subquery's {column_type}",
                                value.data_type
                            ),
                            *span,
                        ))
                    }
                };
                Ok(self.expr(
                    BoundExprKind::Subquery {
                        kind: SubqueryKind::In {
                            expr: Box::new(value),
                            negated: *negated,
                        },
                        plan: Arc::new(plan),
                    },
                    DataType::Boolean,
                    // A NULL on either side makes the answer unknown rather
                    // than false -- the NOT IN trap, now with a subquery
                    // supplying the list.
                    true,
                    *span,
                ))
            }
        }
    }

    fn bind_function(
        &mut self,
        name: &Ident,
        args: &[FunctionArg],
        distinct: bool,
        span: Span,
    ) -> Result<BoundExpr> {
        let Some(func) = AggregateFunction::from_name(&name.value) else {
            return Err(
                Diagnostic::bind(format!("no function named `{}`", name.value), span)
                    .with_hint("scalar functions are not implemented yet"),
            );
        };
        if self.inside_aggregate {
            return Err(Diagnostic::bind(
                "aggregate functions cannot be nested",
                span,
            ));
        }
        let Some(agg_rel) = self.agg.as_ref().map(|a| a.rel) else {
            // `bind_select` decides up front whether the query aggregates, so
            // reaching here means an aggregate appeared somewhere that decision
            // does not look at.
            return Err(Diagnostic::bind(
                format!("aggregate function `{}` is not allowed here", name.value),
                span,
            ));
        };

        // Bind the argument with base columns available again: inside an
        // aggregate is exactly where reading an ungrouped column is correct.
        let arg = match args {
            [FunctionArg::Wildcard(w)] => {
                if func != AggregateFunction::Count {
                    return Err(Diagnostic::bind(
                        format!("`{}` does not accept `*`", func.as_str()),
                        *w,
                    ));
                }
                if distinct {
                    return Err(Diagnostic::bind("`COUNT(DISTINCT *)` is not valid", *w));
                }
                None
            }
            [FunctionArg::Expr(e)] => {
                self.inside_aggregate = true;
                let bound = self.bind_expr(e);
                self.inside_aggregate = false;
                Some(bound?)
            }
            _ => {
                return Err(Diagnostic::bind(
                    format!(
                        "`{}` takes exactly one argument, found {}",
                        func.as_str(),
                        args.len()
                    ),
                    span,
                ))
            }
        };

        let (data_type, nullable) = aggregate_result_type(func, arg.as_ref(), span)?;

        let scope = self.agg.as_mut().expect("checked above");
        let index = scope.group_exprs.len() + scope.aggregates.len();
        let aggregate = BoundAggregate {
            func,
            arg,
            distinct,
            data_type,
            nullable,
            span,
        };
        let label = aggregate.to_sql();
        scope.aggregates.push(aggregate);

        Ok(self.expr(
            BoundExprKind::Column {
                rel: agg_rel,
                index,
                name: label,
            },
            data_type,
            nullable,
            span,
        ))
    }

    /// Whether `e` is written exactly like one of the grouping expressions.
    fn match_group_expr(&self, e: &Expr) -> Option<usize> {
        let scope = self.agg.as_ref()?;
        scope.group_asts.iter().position(|g| ast_eq(g, e))
    }

    /// A reference to grouping key `index` in the aggregate's output.
    fn group_slot(&mut self, index: usize, span: Span) -> BoundExpr {
        let scope = self.agg.as_ref().expect("caller checked");
        let (rel, source) = (scope.rel, scope.group_exprs[index].clone());
        let name = match &source.kind {
            BoundExprKind::Column { name, .. } => name.clone(),
            _ => strip_outer_parens(&source.to_sql()),
        };
        self.expr(
            BoundExprKind::Column { rel, index, name },
            source.data_type,
            source.nullable,
            span,
        )
    }

    fn bind_literal(&mut self, lit: &Literal, span: Span) -> Result<BoundExpr> {
        let value = match lit {
            Literal::Null => ScalarValue::Null,
            Literal::Boolean(b) => ScalarValue::Boolean(*b),
            Literal::String(s) => ScalarValue::Utf8(s.clone()),
            Literal::Number(text) => parse_number_literal(text, span)?,
        };
        let dt = value.data_type();
        let nullable = value.is_null();
        Ok(self.expr(BoundExprKind::Literal(value), dt, nullable, span))
    }

    fn bind_binary(
        &mut self,
        left: &Expr,
        op: BinaryOperator,
        right: &Expr,
        op_span: Span,
        span: Span,
    ) -> Result<BoundExpr> {
        let l = self.bind_expr(left)?;
        let r = self.bind_expr(right)?;

        if op.is_logical() {
            let l = self.require_boolean(l, op.as_str())?;
            let r = self.require_boolean(r, op.as_str())?;
            // Conservative nullability. `false AND NULL` is actually false, so
            // a tighter rule exists, but claiming non-nullable when it might be
            // NULL would license unsound rewrites later.
            let nullable = l.nullable || r.nullable;
            return Ok(self.expr(
                BoundExprKind::Binary { op, left: Box::new(l), right: Box::new(r) },
                DataType::Boolean,
                nullable,
                span,
            ));
        }

        if op == BinaryOperator::StringConcat {
            let l = self.coerce_to(l, DataType::Utf8, op_span)?;
            let r = self.coerce_to(r, DataType::Utf8, op_span)?;
            let nullable = l.nullable || r.nullable;
            return Ok(self.expr(
                BoundExprKind::Binary { op, left: Box::new(l), right: Box::new(r) },
                DataType::Utf8,
                nullable,
                span,
            ));
        }

        let (l, r, common) = self.unify(l, r, op.as_str(), op_span)?;
        let nullable = l.nullable || r.nullable;

        if op.is_comparison() {
            if !common.is_comparable() && common != DataType::Null {
                return Err(Diagnostic::bind(
                    format!("cannot compare values of type {common}"),
                    op_span,
                ));
            }
            return Ok(self.expr(
                BoundExprKind::Binary { op, left: Box::new(l), right: Box::new(r) },
                DataType::Boolean,
                nullable,
                span,
            ));
        }

        // Arithmetic.
        if !common.is_numeric() && common != DataType::Null {
            return Err(Diagnostic::bind(
                format!(
                    "operator `{}` is not defined for {} and {}",
                    op.as_str(),
                    l.data_type,
                    r.data_type
                ),
                op_span,
            ));
        }

        // Narrow storage, wide arithmetic: Int32 operands compute at Int64.
        //
        // Int32 exists so that a column whose values fit in 32 bits costs four
        // bytes each -- it is a storage decision the CSV loader made by looking
        // at the data, not something the user asked for. Evaluating `id *
        // salary` in 32 bits would then overflow on perfectly ordinary data,
        // which is a surprise attributable entirely to an inference detail.
        //
        // Comparisons deliberately do *not* widen. They stay at the column's
        // own width so that zone-map bounds and (later) dictionary codes can be
        // compared in their native representation without a conversion per row.
        let (l, r, common) = if common == DataType::Int32 {
            let l = self.coerce_to(l, DataType::Int64, op_span)?;
            let r = self.coerce_to(r, DataType::Int64, op_span)?;
            (l, r, DataType::Int64)
        } else {
            (l, r, common)
        };

        Ok(self.expr(
            BoundExprKind::Binary { op, left: Box::new(l), right: Box::new(r) },
            common,
            nullable,
            span,
        ))
    }

    fn bind_unary(&mut self, op: UnaryOperator, inner: &Expr, span: Span) -> Result<BoundExpr> {
        let e = self.bind_expr(inner)?;
        match op {
            UnaryOperator::Not => {
                let e = self.require_boolean(e, "NOT")?;
                let nullable = e.nullable;
                Ok(self.expr(
                    BoundExprKind::Unary { op, expr: Box::new(e) },
                    DataType::Boolean,
                    nullable,
                    span,
                ))
            }
            UnaryOperator::Minus | UnaryOperator::Plus => {
                if !e.data_type.is_numeric() && e.data_type != DataType::Null {
                    return Err(Diagnostic::bind(
                        format!("unary `{}` is not defined for {}", op.as_str(), e.data_type),
                        span,
                    ));
                }
                // Fold a sign directly into a numeric literal. The lexer
                // deliberately does not make the sign part of the number token
                // (otherwise `a-1` would lex as `a` and `-1`), so this is the
                // stage that reassembles `-5` into a single constant -- which
                // is what lets LIMIT / OFFSET see a value at all, and what will
                // let a zone map compare against a negative bound later.
                if let BoundExprKind::Literal(v) = &e.kind {
                    if let Some(folded) = fold_signed_literal(op, v) {
                        let dt = folded.data_type();
                        return Ok(self.expr(
                            BoundExprKind::Literal(folded),
                            dt,
                            false,
                            span,
                        ));
                    }
                }
                let (dt, nullable) = (e.data_type, e.nullable);
                Ok(self.expr(
                    BoundExprKind::Unary { op, expr: Box::new(e) },
                    dt,
                    nullable,
                    span,
                ))
            }
        }
    }

    fn bind_between(
        &mut self,
        expr: &Expr,
        negated: bool,
        low: &Expr,
        high: &Expr,
        span: Span,
    ) -> Result<BoundExpr> {
        let e = self.bind_expr(expr)?;
        let l = self.bind_expr(low)?;
        let h = self.bind_expr(high)?;

        // All three operands must land on one type, so unify pairwise and then
        // re-coerce everything to the result.
        let (e, l, t1) = self.unify(e, l, "BETWEEN", span)?;
        let (e, h, t2) = self.unify(e, h, "BETWEEN", span)?;
        let common = types::common_type(t1, t2).ok_or_else(|| {
            Diagnostic::bind(
                format!("BETWEEN bounds have incompatible types {t1} and {t2}"),
                span,
            )
        })?;
        let e = self.coerce_to(e, common, span)?;
        let l = self.coerce_to(l, common, span)?;
        let h = self.coerce_to(h, common, span)?;

        let nullable = e.nullable || l.nullable || h.nullable;
        Ok(self.expr(
            BoundExprKind::Between {
                expr: Box::new(e),
                low: Box::new(l),
                high: Box::new(h),
                negated,
            },
            DataType::Boolean,
            nullable,
            span,
        ))
    }

    fn bind_in_list(
        &mut self,
        expr: &Expr,
        list: &[Expr],
        negated: bool,
        span: Span,
    ) -> Result<BoundExpr> {
        let mut e = self.bind_expr(expr)?;
        let mut items = Vec::with_capacity(list.len());
        let mut common = e.data_type;
        for item in list {
            let bound = self.bind_expr(item)?;
            let (new_e, bound, t) = self.unify(e, bound, "IN", span)?;
            e = new_e;
            common = t;
            items.push(bound);
        }
        // A later item may have widened the common type past what earlier ones
        // were coerced to, so make one more pass with the final answer.
        let e = self.coerce_to(e, common, span)?;
        let mut coerced = Vec::with_capacity(items.len());
        let mut any_null = false;
        for item in items {
            let item = self.coerce_to(item, common, span)?;
            any_null |= item.nullable;
            coerced.push(item);
        }

        // NULL semantics of IN / NOT IN, the classic trap:
        //   `x IN (1, NULL)`     is TRUE if x = 1, else UNKNOWN (never FALSE).
        //   `x NOT IN (1, NULL)` is FALSE if x = 1, else UNKNOWN -- so a NULL
        //   anywhere in the list makes NOT IN match no rows at all.
        // The result is therefore nullable whenever the list can contain NULL,
        // regardless of whether `x` itself is nullable.
        let nullable = e.nullable || any_null;
        Ok(self.expr(
            BoundExprKind::InList { expr: Box::new(e), list: coerced, negated },
            DataType::Boolean,
            nullable,
            span,
        ))
    }

    fn bind_like(
        &mut self,
        expr: &Expr,
        pattern: &Expr,
        negated: bool,
        case_insensitive: bool,
        escape: Option<&Expr>,
        span: Span,
    ) -> Result<BoundExpr> {
        let e = self.bind_expr(expr)?;
        let e = self.require_type(e, DataType::Utf8, "LIKE", span)?;
        let p = self.bind_expr(pattern)?;
        let p = self.require_type(p, DataType::Utf8, "LIKE pattern", span)?;
        let esc = match escape {
            Some(x) => {
                let b = self.bind_expr(x)?;
                Some(Box::new(self.require_type(b, DataType::Utf8, "ESCAPE", span)?))
            }
            None => None,
        };
        let nullable = e.nullable || p.nullable || esc.as_ref().is_some_and(|x| x.nullable);
        Ok(self.expr(
            BoundExprKind::Like {
                expr: Box::new(e),
                pattern: Box::new(p),
                escape: esc,
                negated,
                case_insensitive,
            },
            DataType::Boolean,
            nullable,
            span,
        ))
    }

    fn bind_case(
        &mut self,
        operand: Option<&Expr>,
        when_then: &[(Expr, Expr)],
        else_result: Option<&Expr>,
        span: Span,
    ) -> Result<BoundExpr> {
        // A simple `CASE x WHEN v THEN ...` is rewritten to the searched form
        // `CASE WHEN x = v THEN ...`. Doing it here rather than in evaluation
        // gets the NULL semantics right for free: if `x` is NULL then `x = v`
        // is UNKNOWN, no branch matches, and the result is the ELSE -- which is
        // exactly what the standard requires.
        let bound_operand = match operand {
            Some(o) => Some(self.bind_expr(o)?),
            None => None,
        };

        let mut branches = Vec::with_capacity(when_then.len());
        let mut result_type: Option<DataType> = None;
        let mut nullable = else_result.is_none(); // a missing ELSE yields NULL

        for (when, then) in when_then {
            let cond = self.bind_expr(when)?;
            let cond = match &bound_operand {
                None => self.require_boolean(cond, "CASE WHEN")?,
                Some(op) => {
                    let (l, r, _) = self.unify(op.clone(), cond, "CASE", span)?;
                    self.expr(
                        BoundExprKind::Binary {
                            op: BinaryOperator::Eq,
                            left: Box::new(l),
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                        true,
                        span,
                    )
                }
            };
            let value = self.bind_expr(then)?;
            nullable |= value.nullable;
            result_type = Some(merge_result_type(result_type, value.data_type, span)?);
            branches.push((cond, value));
        }

        let else_expr = match else_result {
            Some(e) => {
                let b = self.bind_expr(e)?;
                nullable |= b.nullable;
                result_type = Some(merge_result_type(result_type, b.data_type, span)?);
                Some(b)
            }
            None => None,
        };

        let result_type = result_type.unwrap_or(DataType::Null);

        // Every branch is cast to the unified result type so that the output
        // column has one physical representation.
        let mut coerced = Vec::with_capacity(branches.len());
        for (cond, value) in branches {
            let value = self.coerce_to(value, result_type, span)?;
            coerced.push((cond, value));
        }
        let else_expr = match else_expr {
            Some(e) => Some(Box::new(self.coerce_to(e, result_type, span)?)),
            None => None,
        };

        Ok(self.expr(
            BoundExprKind::Case { when_then: coerced, else_expr },
            result_type,
            nullable,
            span,
        ))
    }

    // -- name resolution ----------------------------------------------------

    /// Every relation visible from here, innermost frame first.
    fn visible_scope(&self) -> Vec<ScopeEntry> {
        self.scopes.iter().rev().flatten().cloned().collect()
    }

    fn lookup_relation(&self, qualifier: &Ident) -> Result<ScopeEntry> {
        let want = qualifier.normalized();
        // Innermost frame first: an inner relation shadows an outer one of the
        // same name, which is what makes a self-referencing subquery work.
        for frame in self.scopes.iter().rev() {
            let matches: Vec<&ScopeEntry> = frame
                .iter()
                .filter(|e| {
                    if qualifier.quoted {
                        e.binding == want
                    } else {
                        e.binding.eq_ignore_ascii_case(&want)
                    }
                })
                .collect();
            match matches.as_slice() {
                [one] => return Ok((*one).clone()),
                [] => continue,
                _ => {
                    return Err(Diagnostic::bind(
                        format!("table alias `{}` is used more than once", qualifier.value),
                        qualifier.span,
                    ))
                }
            }
        }
        let matches: Vec<&ScopeEntry> = Vec::new();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => {
                let available: Vec<String> = self
                    .visible_scope()
                    .iter()
                    .map(|e| e.binding.clone())
                    .collect();
                let mut d = Diagnostic::bind(
                    format!("no table or alias named `{}` in this query", qualifier.value),
                    qualifier.span,
                );
                if let Some(sug) = closest_match(&want, available.iter().map(|s| s.as_str())) {
                    d = d.with_hint(format!("did you mean `{sug}`?"));
                } else if !available.is_empty() {
                    d = d.with_hint(format!("in scope: {}", available.join(", ")));
                }
                Err(d)
            }
            _ => Err(Diagnostic::bind(
                format!("table alias `{}` is used more than once", qualifier.value),
                qualifier.span,
            )),
        }
    }

    fn resolve_column(&mut self, qualifier: Option<&Ident>, name: &Ident) -> Result<BoundExpr> {
        let wanted = name.normalized();

        // Qualified: exactly one relation to search.
        if let Some(q) = qualifier {
            let entry = self.lookup_relation(q)?;
            return match entry.schema.resolve(&wanted, name.quoted) {
                Resolution::Found(index) => {
                    let resolved = self.column_expr(&entry, index, name.span);
                    self.aggregate_column(resolved, name)
                }
                Resolution::Ambiguous(_) => Err(Diagnostic::bind(
                    format!(
                        "column `{}` matches more than one column of `{}` (differing only in case)",
                        name.value, entry.binding
                    ),
                    name.span,
                )),
                Resolution::NotFound => {
                    let mut d = Diagnostic::bind(
                        format!("no column `{}` in `{}`", name.value, entry.binding),
                        name.span,
                    );
                    if let Some(sug) = closest_match(&wanted, entry.schema.names()) {
                        d = d.with_hint(format!("did you mean `{sug}`?"));
                    }
                    Err(d)
                }
            };
        }

        // Unqualified: search the innermost frame, then outward. More than one
        // hit *within a frame* is an ambiguity error naming the candidates --
        // guessing there would silently change which column a query reads. A
        // hit in an outer frame is a correlated reference, and an inner
        // relation deliberately shadows an outer one.
        let mut hits: Vec<(ScopeEntry, usize)> = Vec::new();
        for frame in self.scopes.iter().rev() {
            for entry in frame {
                match entry.schema.resolve(&wanted, name.quoted) {
                    Resolution::Found(i) => hits.push((entry.clone(), i)),
                    Resolution::Ambiguous(idxs) => {
                        for i in idxs {
                            hits.push((entry.clone(), i));
                        }
                    }
                    Resolution::NotFound => {}
                }
            }
            if !hits.is_empty() {
                break;
            }
        }

        match hits.len() {
            1 => {
                let (entry, index) = hits.remove(0);
                let resolved = self.column_expr(&entry, index, name.span);
                self.aggregate_column(resolved, name)
            }
            0 => {
                let visible = self.visible_scope();
                if visible.is_empty() {
                    return Err(Diagnostic::bind(
                        format!("no column `{}`: this query has no FROM clause", name.value),
                        name.span,
                    ));
                }
                let all: Vec<String> = visible
                    .iter()
                    .flat_map(|e| e.schema.names().map(str::to_string))
                    .collect();
                let mut d = Diagnostic::bind(
                    format!("no such column `{}`", name.value),
                    name.span,
                );
                if let Some(sug) = closest_match(&wanted, all.iter().map(|s| s.as_str())) {
                    d = d.with_hint(format!("did you mean `{sug}`?"));
                }
                Err(d)
            }
            _ => {
                let sources: Vec<String> = hits
                    .iter()
                    .map(|(e, i)| format!("`{}.{}`", e.binding, e.schema.field(*i).name))
                    .collect();
                Err(Diagnostic::bind(
                    format!(
                        "column `{}` is ambiguous between {}",
                        name.value,
                        sources.join(" and ")
                    ),
                    name.span,
                )
                .with_hint("qualify it with the table name or alias"))
            }
        }
    }

    fn column_expr(&mut self, entry: &ScopeEntry, index: usize, span: Span) -> BoundExpr {
        let field = entry.schema.field(index);
        let kind = BoundExprKind::Column {
            rel: entry.rel,
            index,
            name: field.name.clone(),
        };
        let (dt, nullable) = (field.data_type, field.nullable);
        self.expr(kind, dt, nullable, span)
    }

    /// In an aggregate query, a resolved base column is only legal if it is one
    /// of the grouping keys. Matching on the resolved `(rel, index)` rather
    /// than on syntax is what makes `SELECT t.a ... GROUP BY a` work.
    fn aggregate_column(&mut self, resolved: BoundExpr, name: &Ident) -> Result<BoundExpr> {
        let Some(scope) = self.agg.as_ref() else {
            return Ok(resolved);
        };
        if self.inside_aggregate {
            return Ok(resolved);
        }
        let BoundExprKind::Column { rel, index, .. } = &resolved.kind else {
            return Ok(resolved);
        };
        let slot = scope.group_exprs.iter().position(|g| {
            matches!(&g.kind, BoundExprKind::Column { rel: r, index: i, .. } if r == rel && i == index)
        });
        match slot {
            Some(i) => Ok(self.group_slot(i, resolved.span)),
            None => {
                let mut d = Diagnostic::bind(
                    format!(
                        "column `{}` must appear in the GROUP BY clause or be used in an aggregate function",
                        name.value
                    ),
                    resolved.span,
                );
                if scope.group_exprs.is_empty() {
                    d = d.with_hint(
                        "this query aggregates over all rows, so it produces a single row \
                         with no place for a per-row column",
                    );
                }
                Err(d)
            }
        }
    }

    // -- type checking helpers ----------------------------------------------

    fn require_boolean(&mut self, e: BoundExpr, context: &str) -> Result<BoundExpr> {
        match e.data_type {
            DataType::Boolean => Ok(e),
            // A bare NULL is a valid boolean of unknown value.
            DataType::Null => {
                let span = e.span;
                self.coerce_to(e, DataType::Boolean, span)
            }
            other => Err(Diagnostic::bind(
                format!("{context} requires a boolean expression, found {other}"),
                e.span,
            )),
        }
    }

    fn require_type(&mut self, e: BoundExpr, want: DataType, context: &str, span: Span) -> Result<BoundExpr> {
        if e.data_type == want || e.data_type == DataType::Null {
            return self.coerce_to(e, want, span);
        }
        Err(Diagnostic::bind(
            format!("{context} requires {want}, found {}", e.data_type),
            e.span,
        ))
    }

    /// Insert an explicit cast if the value is not already of the target type.
    ///
    /// Two kinds of conversion are allowed here:
    ///   * a widening cast that cannot fail (Int32 -> Int64, Date32 -> Timestamp);
    ///   * folding a *string literal* into the target type, which is what makes
    ///     `WHERE d > '2024-01-01'` and `WHERE n = '5'` work.
    ///
    /// Literal folding is restricted to literals on purpose: a genuine type
    /// mismatch between two *columns* stays an error instead of turning into a
    /// silent per-row conversion that fails halfway through a scan.
    fn coerce_to(&mut self, e: BoundExpr, target: DataType, span: Span) -> Result<BoundExpr> {
        if e.data_type == target {
            return Ok(e);
        }

        if let BoundExprKind::Literal(v) = &e.kind {
            if !v.is_null() {
                let folded = types::cast_scalar(v, &target).map_err(|_| {
                    Diagnostic::bind(
                        format!("`{v}` is not a valid {target}"),
                        e.span,
                    )
                })?;
                let dt = folded.data_type();
                return Ok(self.expr(BoundExprKind::Literal(folded), dt, false, e.span));
            }
            // A NULL literal simply takes on the target type.
            return Ok(self.expr(
                BoundExprKind::Literal(ScalarValue::Null),
                target,
                true,
                e.span,
            ));
        }

        if !types::can_widen(e.data_type, target) && target != DataType::Utf8 {
            return Err(Diagnostic::bind(
                format!("cannot implicitly convert {} to {target}", e.data_type),
                span,
            )
            .with_hint("use an explicit CAST"));
        }

        let nullable = e.nullable;
        Ok(self.expr(
            BoundExprKind::Cast { expr: Box::new(e), implicit: true },
            target,
            nullable,
            span,
        ))
    }

    /// Bring two operands to a common type, inserting casts.
    fn unify(
        &mut self,
        l: BoundExpr,
        r: BoundExpr,
        context: &str,
        span: Span,
    ) -> Result<(BoundExpr, BoundExpr, DataType)> {
        if let Some(common) = types::common_type(l.data_type, r.data_type) {
            let l = self.coerce_to(l, common, span)?;
            let r = self.coerce_to(r, common, span)?;
            return Ok((l, r, common));
        }

        // No common type, but a string literal can still be folded into the
        // other side. `d > '2024-01-01'` and `'5' < n` both land here.
        let l_is_str_lit = is_string_literal(&l);
        let r_is_str_lit = is_string_literal(&r);
        if r_is_str_lit && !l_is_str_lit {
            let target = l.data_type;
            let r = self.coerce_to(r, target, span)?;
            return Ok((l, r, target));
        }
        if l_is_str_lit && !r_is_str_lit {
            let target = r.data_type;
            let l = self.coerce_to(l, target, span)?;
            return Ok((l, r, target));
        }

        Err(Diagnostic::bind(
            format!(
                "{context} cannot combine {} and {}",
                l.data_type, r.data_type
            ),
            span,
        )
        .with_hint("use an explicit CAST"))
    }
}

/// Apply a leading sign to a numeric literal, or `None` if it does not apply
/// (non-numeric, or a negation that would overflow).
fn fold_signed_literal(op: UnaryOperator, v: &ScalarValue) -> Option<ScalarValue> {
    if op == UnaryOperator::Plus {
        return match v {
            ScalarValue::Int32(_) | ScalarValue::Int64(_) | ScalarValue::Float64(_) => {
                Some(v.clone())
            }
            _ => None,
        };
    }
    match v {
        ScalarValue::Int32(i) => i.checked_neg().map(ScalarValue::Int32),
        ScalarValue::Int64(i) => i.checked_neg().map(ScalarValue::Int64),
        ScalarValue::Float64(f) => Some(ScalarValue::Float64(-f)),
        _ => None,
    }
}

/// Relations whose columns appear in a plan's output.
/// Two `OVER (...)` clauses are the same window when they are written the same
/// way -- the same syntactic rule that matches a GROUP BY key.
fn window_spec_eq(
    a: &crate::parser::ast::WindowSpec,
    b: &crate::parser::ast::WindowSpec,
) -> bool {
    a.partition_by.len() == b.partition_by.len()
        && a.partition_by
            .iter()
            .zip(&b.partition_by)
            .all(|(x, y)| ast_eq(x, y))
        && a.order_by.len() == b.order_by.len()
        && a.order_by.iter().zip(&b.order_by).all(|(x, y)| {
            ast_eq(&x.expr, &y.expr) && x.asc == y.asc && x.nulls_first == y.nulls_first
        })
}

/// Turn the parsed frame clause into resolved offsets.
fn resolve_frame(
    spec: &crate::parser::ast::WindowSpec,
    func: WindowFunction,
    span: Span,
) -> Result<Frame> {
    let Some(frame) = &spec.frame else {
        return Ok(Frame::default_for(!spec.order_by.is_empty()));
    };
    if func.ignores_frame() {
        // A rank is a position, not a window over values.
        return Ok(Frame::default_for(!spec.order_by.is_empty()));
    }

    let units = match frame.units {
        crate::parser::ast::FrameUnits::Rows => FrameUnits::Rows,
        crate::parser::ast::FrameUnits::Range => FrameUnits::Range,
    };
    let bound = |b: &crate::parser::ast::FrameBound| -> Result<FrameBound> {
        Ok(match b {
            crate::parser::ast::FrameBound::UnboundedPreceding => FrameBound::UnboundedPreceding,
            crate::parser::ast::FrameBound::UnboundedFollowing => FrameBound::UnboundedFollowing,
            crate::parser::ast::FrameBound::CurrentRow => FrameBound::CurrentRow,
            crate::parser::ast::FrameBound::Preceding(e) => {
                FrameBound::Preceding(frame_offset(e, units, span)?)
            }
            crate::parser::ast::FrameBound::Following(e) => {
                FrameBound::Following(frame_offset(e, units, span)?)
            }
        })
    };
    Ok(Frame {
        units,
        start: bound(&frame.start)?,
        end: bound(&frame.end)?,
    })
}

fn frame_offset(e: &Expr, units: FrameUnits, span: Span) -> Result<usize> {
    if units == FrameUnits::Range {
        // A RANGE offset is measured in the ORDER BY column's own units, which
        // needs per-type arithmetic on the bound. Only the peer-based bounds
        // are supported.
        return Err(Diagnostic::bind(
            "a RANGE frame with a numeric offset is not supported yet",
            span,
        )
        .with_hint("use ROWS for a count of rows, or an unbounded/current-row bound"));
    }
    let Expr::Literal { value: Literal::Number(text), .. } = e else {
        return Err(Diagnostic::bind("a frame offset must be a constant", span));
    };
    text.parse::<usize>()
        .map_err(|_| Diagnostic::bind("a frame offset must be a non-negative integer", span))
}

/// Result type of a window function.
fn window_result_type(
    func: WindowFunction,
    args: &[BoundExpr],
    span: Span,
) -> Result<(DataType, bool)> {
    match func {
        // A position always exists.
        WindowFunction::RowNumber | WindowFunction::Rank | WindowFunction::DenseRank => {
            if !args.is_empty() {
                return Err(Diagnostic::bind(
                    format!("`{}` takes no arguments", func.as_str()),
                    span,
                ));
            }
            Ok((DataType::Int64, false))
        }
        WindowFunction::Lag | WindowFunction::Lead => {
            let Some(first) = args.first() else {
                return Err(Diagnostic::bind(
                    format!("`{}` needs an expression to shift", func.as_str()),
                    span,
                ));
            };
            if args.len() > 3 {
                return Err(Diagnostic::bind(
                    format!("`{}` takes at most three arguments", func.as_str()),
                    span,
                ));
            }
            // Nullable even over a non-nullable column: the rows at the edge of
            // a partition have no neighbour to read.
            Ok((first.data_type, true))
        }
        WindowFunction::Aggregate(a) => {
            aggregate_result_type(a, args.first(), span)
        }
    }
}

fn collect_relations(plan: &LogicalPlan, out: &mut Vec<RelId>) {
    match plan {
        LogicalPlan::Scan { rel, .. }
        | LogicalPlan::Aggregate { rel, .. }
        | LogicalPlan::CteRef { rel, .. }
        | LogicalPlan::SubqueryAlias { rel, .. } => out.push(*rel),
        LogicalPlan::Sort { input, .. } | LogicalPlan::Distinct { input, .. } => {
            collect_relations(input, out)
        }
        LogicalPlan::Window { rel, input, .. } => {
            collect_relations(input, out);
            out.push(*rel);
        }
        LogicalPlan::SetOp { .. } => {}
        LogicalPlan::OneRow { .. } | LogicalPlan::Project { .. } => {}
        LogicalPlan::Filter { input, .. } | LogicalPlan::Limit { input, .. } => {
            collect_relations(input, out)
        }
        LogicalPlan::Join { left, right, .. } => {
            collect_relations(left, out);
            collect_relations(right, out);
        }
    }
}

/// Result type and nullability of an aggregate.
///
/// COUNT is the odd one out: it counts rather than combining values, so it is
/// an INT64 that is never NULL -- including over an empty input, where it is 0
/// while every other aggregate is NULL. That asymmetry is a classic source of
/// wrong answers and it is decided here, once.
fn aggregate_result_type(
    func: AggregateFunction,
    arg: Option<&BoundExpr>,
    span: Span,
) -> Result<(DataType, bool)> {
    use AggregateFunction::*;
    let arg_type = arg.map(|a| a.data_type).unwrap_or(DataType::Null);
    Ok(match func {
        Count => (DataType::Int64, false),
        Sum => {
            let dt = match arg_type {
                DataType::Int32 | DataType::Int64 => DataType::Int64,
                DataType::Float64 => DataType::Float64,
                DataType::Null => DataType::Null,
                other => {
                    return Err(Diagnostic::bind(
                        format!("SUM is not defined for {other}"),
                        span,
                    ))
                }
            };
            // NULL over an empty group, or over a group whose values are all
            // NULL -- so always nullable regardless of the input column.
            (dt, true)
        }
        Avg => {
            if !arg_type.is_numeric() && arg_type != DataType::Null {
                return Err(Diagnostic::bind(
                    format!("AVG is not defined for {arg_type}"),
                    span,
                ));
            }
            (DataType::Float64, true)
        }
        Min | Max => {
            if !arg_type.is_comparable() && arg_type != DataType::Null {
                return Err(Diagnostic::bind(
                    format!("{} is not defined for {arg_type}", func.as_str()),
                    span,
                ));
            }
            (arg_type, true)
        }
    })
}

/// Whether an aggregate call appears anywhere in an expression. Used to decide
/// up front whether a query aggregates, before any binding happens.
fn expr_has_aggregate(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |node| {
        // `SUM(x) OVER (...)` is a window function, not a grouping aggregate:
        // it produces a value per row rather than collapsing the query to one.
        // Counting it here would silently turn a windowed query into an
        // aggregated one.
        if let Expr::Function { name, over: None, .. } = node {
            if AggregateFunction::from_name(&name.value).is_some() {
                found = true;
            }
        }
    });
    found
}

fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Literal { .. } => {}
        Expr::BinaryOp { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. } => walk_expr(expr, f),
        Expr::Between { expr, low, high, .. } => {
            walk_expr(expr, f);
            walk_expr(low, f);
            walk_expr(high, f);
        }
        Expr::InList { expr, list, .. } => {
            walk_expr(expr, f);
            for e in list {
                walk_expr(e, f);
            }
        }
        Expr::Like { expr, pattern, escape, .. } => {
            walk_expr(expr, f);
            walk_expr(pattern, f);
            if let Some(e) = escape {
                walk_expr(e, f);
            }
        }
        Expr::Case { operand, when_then, else_result, .. } => {
            if let Some(o) = operand {
                walk_expr(o, f);
            }
            for (w, t) in when_then {
                walk_expr(w, f);
                walk_expr(t, f);
            }
            if let Some(e) = else_result {
                walk_expr(e, f);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                if let FunctionArg::Expr(e) = a {
                    walk_expr(e, f);
                }
            }
        }
        // Deliberately not descending into a subquery: an aggregate inside one
        // aggregates *that* query, and treating it as the outer query's would
        // collapse the wrong thing to a single row.
        Expr::ScalarSubquery { .. } | Expr::Exists { .. } => {}
        Expr::InSubquery { expr, .. } => walk_expr(expr, f),
    }
}

/// Structural equality on parsed expressions, ignoring source positions.
///
/// This is how a grouping key is recognised in the SELECT list. It is
/// deliberately syntactic, which is what the standard describes and what other
/// engines do: `GROUP BY a + 1` matches `SELECT a + 1`, but not `SELECT 1 + a`.
/// Parentheses are transparent, since they carry no meaning past parsing.
fn ast_eq(a: &Expr, b: &Expr) -> bool {
    // Unwrap parentheses on either side before comparing.
    if let Expr::Nested { expr, .. } = a {
        return ast_eq(expr, b);
    }
    if let Expr::Nested { expr, .. } = b {
        return ast_eq(a, expr);
    }
    match (a, b) {
        (Expr::Identifier(x), Expr::Identifier(y)) => x.normalized() == y.normalized(),
        (Expr::CompoundIdentifier(x), Expr::CompoundIdentifier(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(p, q)| p.normalized() == q.normalized())
        }
        (Expr::Literal { value: x, .. }, Expr::Literal { value: y, .. }) => x == y,
        (
            Expr::BinaryOp { left: l1, op: o1, right: r1, .. },
            Expr::BinaryOp { left: l2, op: o2, right: r2, .. },
        ) => o1 == o2 && ast_eq(l1, l2) && ast_eq(r1, r2),
        (Expr::UnaryOp { op: o1, expr: e1, .. }, Expr::UnaryOp { op: o2, expr: e2, .. }) => {
            o1 == o2 && ast_eq(e1, e2)
        }
        (
            Expr::IsNull { expr: e1, negated: n1, .. },
            Expr::IsNull { expr: e2, negated: n2, .. },
        ) => n1 == n2 && ast_eq(e1, e2),
        (
            Expr::Between { expr: e1, negated: n1, low: l1, high: h1, .. },
            Expr::Between { expr: e2, negated: n2, low: l2, high: h2, .. },
        ) => n1 == n2 && ast_eq(e1, e2) && ast_eq(l1, l2) && ast_eq(h1, h2),
        (
            Expr::InList { expr: e1, list: v1, negated: n1, .. },
            Expr::InList { expr: e2, list: v2, negated: n2, .. },
        ) => {
            n1 == n2
                && ast_eq(e1, e2)
                && v1.len() == v2.len()
                && v1.iter().zip(v2).all(|(p, q)| ast_eq(p, q))
        }
        (
            Expr::Like { expr: e1, pattern: p1, negated: n1, case_insensitive: c1, escape: s1, .. },
            Expr::Like { expr: e2, pattern: p2, negated: n2, case_insensitive: c2, escape: s2, .. },
        ) => {
            n1 == n2
                && c1 == c2
                && ast_eq(e1, e2)
                && ast_eq(p1, p2)
                && match (s1, s2) {
                    (None, None) => true,
                    (Some(x), Some(y)) => ast_eq(x, y),
                    _ => false,
                }
        }
        (
            Expr::Cast { expr: e1, data_type: d1, .. },
            Expr::Cast { expr: e2, data_type: d2, .. },
        ) => d1 == d2 && ast_eq(e1, e2),
        (
            Expr::Case { operand: o1, when_then: w1, else_result: r1, .. },
            Expr::Case { operand: o2, when_then: w2, else_result: r2, .. },
        ) => {
            opt_ast_eq(o1.as_deref(), o2.as_deref())
                && w1.len() == w2.len()
                && w1
                    .iter()
                    .zip(w2)
                    .all(|((a1, b1), (a2, b2))| ast_eq(a1, a2) && ast_eq(b1, b2))
                && opt_ast_eq(r1.as_deref(), r2.as_deref())
        }
        (
            Expr::Function { name: n1, args: a1, distinct: d1, .. },
            Expr::Function { name: n2, args: a2, distinct: d2, .. },
        ) => {
            d1 == d2
                && n1.value.eq_ignore_ascii_case(&n2.value)
                && a1.len() == a2.len()
                && a1.iter().zip(a2).all(|(p, q)| match (p, q) {
                    (FunctionArg::Wildcard(_), FunctionArg::Wildcard(_)) => true,
                    (FunctionArg::Expr(x), FunctionArg::Expr(y)) => ast_eq(x, y),
                    _ => false,
                })
        }
        // Two subqueries are never treated as the same expression, so a
        // subquery can never be a grouping key matched by syntax.
        _ => false,
    }
}

fn opt_ast_eq(a: Option<&Expr>, b: Option<&Expr>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => ast_eq(x, y),
        _ => false,
    }
}

fn is_string_literal(e: &BoundExpr) -> bool {
    matches!(&e.kind, BoundExprKind::Literal(ScalarValue::Utf8(_)))
}

fn merge_result_type(current: Option<DataType>, next: DataType, span: Span) -> Result<DataType> {
    match current {
        None => Ok(next),
        Some(cur) => types::common_type(cur, next).ok_or_else(|| {
            Diagnostic::bind(
                format!("CASE branches have incompatible types {cur} and {next}"),
                span,
            )
        }),
    }
}

/// An integer literal that fits in i32 becomes Int32, otherwise Int64;
/// anything with a decimal point or exponent becomes Float64. Choosing the
/// narrowest integer keeps `WHERE small_int_col = 5` from forcing a widening
/// cast on the column side.
fn parse_number_literal(text: &str, span: Span) -> Result<ScalarValue> {
    if !text.contains(['.', 'e', 'E']) {
        if let Ok(i) = text.parse::<i32>() {
            return Ok(ScalarValue::Int32(i));
        }
        if let Ok(i) = text.parse::<i64>() {
            return Ok(ScalarValue::Int64(i));
        }
    }
    text.parse::<f64>()
        .map(ScalarValue::Float64)
        .map_err(|_| Diagnostic::bind(format!("invalid numeric literal `{text}`"), span))
}

fn strip_outer_parens(s: &str) -> String {
    let t = s.trim();
    if !(t.starts_with('(') && t.ends_with(')')) {
        return t.to_string();
    }
    // Only strip if the opening paren actually matches the closing one.
    let mut depth = 0i32;
    for (i, c) in t.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return if i == t.len() - 1 {
                        t[1..t.len() - 1].to_string()
                    } else {
                        t.to_string()
                    };
                }
            }
            _ => {}
        }
    }
    t.to_string()
}
