//! An analytical SQL query engine, built from scratch.
//!
//! The pipeline, with one module per stage:
//!
//! ```text
//!   SQL text
//!     -> lexer    tokens, each carrying a byte span
//!     -> parser   AST (untyped, close to the source)
//!     -> binder   names resolved, types checked, typed logical plan
//!     -> plan     relational algebra
//!     -> optimizer  rule-based rewrites, every application recorded
//!     -> exec     pull-based batched operators
//!     -> Batch    columnar results
//! ```
//!
//! This crate has no dependencies and no wasm-specific code: it must build and
//! run natively so the CLI stays the primary development surface. The wasm
//! boundary will be a separate crate that wraps `Engine`.
//!
//! `plan()` returns the optimized plan; `bound_plan()` returns it exactly as
//! the binder produced it, and `optimizer_trace()` returns every rewrite in
//! between.

pub mod binder;
pub mod catalog;
pub mod error;
pub mod exec;
pub mod expr;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod plan;
pub mod sqllogictest;
pub mod storage;
pub mod types;

use std::sync::Arc;

use crate::catalog::Catalog;
use crate::error::Result;
use crate::exec::StatsNode;
use crate::lexer::Token;
use crate::optimizer::{Optimizer, OptimizerTrace};
use crate::parser::ast::Statement;
use crate::plan::LogicalPlan;
use crate::storage::{Batch, CsvOptions, Schema, Table};
use crate::types::ScalarValue;

/// The results of one query, plus what it cost to produce them.
#[derive(Debug)]
pub struct QueryResult {
    pub schema: Arc<Schema>,
    pub batches: Vec<Batch>,
    /// Per-operator counters, snapshotted after execution.
    pub stats: StatsNode,
    /// Total wall time for planning plus execution.
    pub elapsed_nanos: u64,
}

impl QueryResult {
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(|b| b.num_rows()).sum()
    }

    /// Materialize every row as scalars. Convenient for the CLI and for tests;
    /// the browser will read typed-array views out of wasm memory instead of
    /// paying for this.
    pub fn rows(&self) -> Vec<Vec<ScalarValue>> {
        let mut out = Vec::with_capacity(self.num_rows());
        for b in &self.batches {
            for row in 0..b.num_rows() {
                out.push((0..b.columns().len()).map(|c| b.value(c, row)).collect());
            }
        }
        out
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_nanos as f64 / 1_000_000.0
    }
}

/// A query in progress, handing back one batch at a time.
///
/// Holds the operator tree, so pulling from it is the same work `execute` does
/// -- just interruptible. The statistics are readable at any point and describe
/// what has flowed so far, which is what makes a progress indicator possible.
pub struct QueryStream {
    root: Box<dyn exec::Operator>,
    schema: Arc<Schema>,
    timer: exec::Timer,
    elapsed_nanos: u64,
    done: bool,
}

impl QueryStream {
    pub fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    /// The next batch, or `None` when the query is finished.
    ///
    /// Not `next`: this is fallible, so it cannot be `Iterator`, and a method
    /// that looks like one but is not is worse than a longer name.
    pub fn next_batch(&mut self) -> Result<Option<Batch>> {
        if self.done {
            return Ok(None);
        }
        let batch = self.root.next()?;
        if batch.is_none() {
            self.done = true;
            self.elapsed_nanos = self.timer.elapsed_nanos();
        }
        Ok(batch)
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Per-operator counters as they stand right now. Partway through a query
    /// these describe the rows that have flowed so far, not the whole answer.
    pub fn stats(&self) -> StatsNode {
        exec::snapshot_stats(self.root.as_ref())
    }

    pub fn elapsed_nanos(&self) -> u64 {
        if self.done {
            self.elapsed_nanos
        } else {
            self.timer.elapsed_nanos()
        }
    }
}

/// The engine: a catalog plus the pipeline that runs against it.
#[derive(Default)]
pub struct Engine {
    catalog: Catalog,
    optimizer: Optimizer,
}

impl Engine {
    pub fn new() -> Engine {
        Engine::default()
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    /// Load a CSV buffer as a table. Data always arrives as bytes -- there is
    /// no filesystem in the browser, so the storage layer never sees a path.
    pub fn load_csv(&mut self, name: &str, bytes: &[u8], opts: &CsvOptions) -> Result<Arc<Table>> {
        let table = storage::read_csv(name, bytes, opts)?;
        Ok(self.catalog.register(table))
    }

    /// Load a Parquet buffer as a table.
    ///
    /// Only the footer is read here; column chunks are decoded the first time a
    /// scan asks for one. The buffer is retained behind an `Arc` because those
    /// later reads slice into it.
    pub fn load_parquet(&mut self, name: &str, bytes: Arc<[u8]>) -> Result<Arc<Table>> {
        let table = storage::parquet::read_parquet(name, bytes)?;
        Ok(self.catalog.register(table))
    }

    /// Load a buffer, choosing the reader by what the bytes say they are.
    ///
    /// Sniffing beats trusting the file extension: the browser will be handed
    /// buffers from a CDN with no name attached, and a Parquet file announces
    /// itself in its first four bytes.
    pub fn load(&mut self, name: &str, bytes: Vec<u8>, opts: &CsvOptions) -> Result<Arc<Table>> {
        if bytes.starts_with(b"PAR1") {
            self.load_parquet(name, bytes.into())
        } else {
            self.load_csv(name, &bytes, opts)
        }
    }

    pub fn register_table(&mut self, table: Table) -> Arc<Table> {
        self.catalog.register(table)
    }

    /// Build a B+ tree index over one column.
    ///
    /// Explicit rather than automatic: an index costs a pass over the column
    /// and helps only a selective predicate, so which column deserves one is a
    /// judgement the engine has no business making on its own.
    pub fn create_index(
        &mut self,
        table: &str,
        column: &str,
    ) -> Result<Arc<catalog::TableIndex>> {
        self.catalog.create_index(table, column)
    }

    // -- pipeline stages, each individually inspectable ---------------------

    pub fn tokenize(&self, sql: &str) -> Result<Vec<Token>> {
        lexer::tokenize(sql)
    }

    pub fn parse(&self, sql: &str) -> Result<Statement> {
        parser::parse(sql)
    }

    /// Parse and bind, with no rewriting. This is the plan the UI's BOUND PLAN
    /// tab shows, and the baseline every rewrite has to agree with.
    pub fn bound_plan(&self, sql: &str) -> Result<LogicalPlan> {
        let stmt = self.parse(sql)?;
        binder::bind(&self.catalog, &stmt)
    }

    /// The optimized plan.
    pub fn plan(&self, sql: &str) -> Result<LogicalPlan> {
        self.optimize(self.bound_plan(sql)?, &self.optimizer)
    }

    /// The optimized plan plus every rule application that produced it.
    pub fn optimizer_trace(&self, sql: &str) -> Result<OptimizerTrace> {
        let (_, trace) = self
            .optimizer
            .optimize_traced(self.bound_plan(sql)?, &self.catalog);
        Ok(trace)
    }

    /// Run a plan through an optimizer, surfacing anything it could not do.
    ///
    /// Two things can go wrong. A mandatory rule may report that it cannot
    /// handle a shape, and a subquery may survive because no rule recognised
    /// it -- either way the plan is not executable, and saying so here beats
    /// an internal error from the evaluator.
    fn optimize(&self, plan: LogicalPlan, optimizer: &Optimizer) -> Result<LogicalPlan> {
        let (plan, trace) = optimizer.optimize_traced(plan, &self.catalog);
        if let Some(error) = trace.error {
            return Err(error);
        }
        if let Some(subquery) = plan.find_subquery() {
            let label = match &subquery.kind {
                crate::plan::BoundExprKind::Subquery { kind, .. } => kind.label(),
                _ => "subquery",
            };
            return Err(crate::error::Diagnostic::bind(
                format!("this {label} subquery is not supported here"),
                subquery.span,
            )
            .with_hint(
                "subqueries are supported uncorrelated anywhere, and correlated as EXISTS or \
                 IN in a WHERE clause",
            ));
        }
        Ok(plan)
    }

    /// The physical plan: which operator was chosen at each node, and why.
    ///
    /// Builds the operator tree without running it, so the reasons are exactly
    /// the ones execution would act on. Row counts are all zero here -- nothing
    /// has flowed yet -- but the estimates are populated, which is what a plan
    /// is decided on.
    pub fn physical_plan(&self, sql: &str) -> Result<exec::StatsNode> {
        self.physical_plan_with(sql, &exec::ExecOptions::default())
    }

    pub fn physical_plan_with(
        &self,
        sql: &str,
        options: &exec::ExecOptions,
    ) -> Result<exec::StatsNode> {
        let plan = self.optimized_for(sql, options)?;
        let root = exec::build_with(&plan, &self.catalog, options)?;
        Ok(exec::snapshot_stats(root.as_ref()))
    }

    /// Execute, handing back one batch at a time.
    ///
    /// The whole-result [`execute`](Self::execute) drains the operator tree
    /// into a `Vec`; this hands the tree back so a caller can pull batches as
    /// it wants them and show progress in between. In the browser that is the
    /// difference between a frozen tab and a row count that climbs.
    pub fn execute_streaming(&self, sql: &str) -> Result<QueryStream> {
        self.execute_streaming_with(sql, &exec::ExecOptions::default())
    }

    pub fn execute_streaming_with(
        &self,
        sql: &str,
        options: &exec::ExecOptions,
    ) -> Result<QueryStream> {
        let timer = exec::Timer::start();
        let plan = self.optimized_for(sql, options)?;
        let root = exec::build_with(&plan, &self.catalog, options)?;
        Ok(QueryStream {
            schema: root.schema(),
            root,
            timer,
            elapsed_nanos: 0,
            done: false,
        })
    }

    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.execute_with(sql, &exec::ExecOptions::default())
    }

    /// Execute with a specific evaluator and batching configuration. Used by
    /// the benchmark and by the test that demands the scalar and vectorized
    /// evaluators agree on every query.
    /// The plan `options` would run, optimized exactly as execution would do it.
    ///
    /// Shared by execution, streaming and the physical plan, so all three
    /// necessarily agree about what is being run.
    fn optimized_for(&self, sql: &str, options: &exec::ExecOptions) -> Result<LogicalPlan> {
        let plan = self.bound_plan(sql)?;
        // Even "unoptimized" runs the mandatory rules: a subquery expression
        // has no execution strategy, so something has to remove it.
        if options.optimize
            && options.reorder_joins
            && options.decorrelate
            && options.aggregate_pushdown
        {
            return self.optimize(plan, &self.optimizer);
        }
        let mut rules: Vec<Box<dyn optimizer::Rule>> = Vec::new();
        if options.decorrelate {
            rules.push(Box::new(optimizer::rules::Decorrelate));
        }
        rules.push(Box::new(optimizer::rules::EvaluateSubqueries));
        if options.optimize {
            rules.push(Box::new(optimizer::rules::ConstantFolding));
            rules.push(Box::new(optimizer::rules::PredicatePushdown));
            rules.push(Box::new(optimizer::rules::LimitPushdown));
            rules.push(Box::new(optimizer::rules::PredicateSimplification));
            rules.push(Box::new(optimizer::rules::OuterToInner));
            rules.push(Box::new(optimizer::rules::CommonSubexpression));
            if options.aggregate_pushdown {
                rules.push(Box::new(optimizer::rules::AggregatePushdown));
            }
            if options.reorder_joins {
                rules.push(Box::new(optimizer::rules::JoinReorder));
            }
            rules.push(Box::new(optimizer::rules::ProjectionPushdown));
        }
        self.optimize(plan, &Optimizer::with_rules(rules))
    }

    pub fn execute_with(&self, sql: &str, options: &exec::ExecOptions) -> Result<QueryResult> {
        let timer = exec::Timer::start();
        let plan = self.optimized_for(sql, options)?;
        let mut root = exec::build_with(&plan, &self.catalog, options)?;
        let schema = root.schema();
        let batches = exec::collect(root.as_mut())?;
        let stats = exec::snapshot_stats(root.as_ref());
        Ok(QueryResult {
            schema,
            batches,
            stats,
            elapsed_nanos: timer.elapsed_nanos(),
        })
    }
}

#[cfg(test)]
mod tests;
