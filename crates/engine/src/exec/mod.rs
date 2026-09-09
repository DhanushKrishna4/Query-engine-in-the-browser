//! Physical operators and execution.
//!
//! The model is pull-based Volcano, but batched: `next()` hands back a whole
//! `Batch` rather than a row, so the per-call overhead of the iterator chain
//! amortizes across ~2048 rows instead of being paid once per row.
//!
//! Two properties are deliberately preserved for later work:
//!
//!   * **Parallelism.** Every operator owns its own state and reaches shared
//!     data only through `Arc`. A scan is already partitioned by row group, so
//!     splitting one scan into N per-partition scans feeding an exchange
//!     operator does not require touching this interface.
//!   * **Instrumentation.** Every operator records rows in/out, batches, wall
//!     time and peak batch size from the start. Timing is *exclusive*: a
//!     parent stops its own clock while pulling from its child, so the numbers
//!     sum to the total instead of nesting.
//!
//! There are three real physical choices, each made on a measured threshold
//! rather than a rule: hash versus nested-loop join, a bounded top-N heap
//! versus a full sort, and -- widest of the three -- how a filtered scan reads
//! its rows. A scan may walk a B+ tree and gather exactly the rows it names
//! (`exec::index`), read every row group with the zone maps and bloom filters
//! skipping what they can prove empty (`exec::prune`), or read everything.
//! All three are safe for the same reason: the filter above still evaluates
//! the predicate, so the choice decides only which rows are *read*.

pub mod aggregate;
pub mod index;
pub mod join;
pub mod key;
pub mod prune;
pub mod setop;
pub mod sort;
pub mod window;

use std::collections::BTreeSet;
use std::sync::Arc;

pub use aggregate::{CompiledAggregate, HashAggregateExec};
pub use index::{IndexScanExec, IndexRange};
pub use join::{HashJoinExec, NestedLoopJoinExec};
pub use setop::{DistinctExec, SetOpExec};
pub use sort::{CompiledSortKey, SortExec, TopNExec};
pub use window::{CompiledWindowFunction, WindowExec};

use crate::catalog::Catalog;
use crate::error::{Diagnostic, Result};
use crate::expr::{self, CompiledExpr, EvalContext};
use crate::parser::ast::BinaryOperator;
use crate::plan::{BoundExpr, BoundExprKind, JoinType, LogicalPlan, RelId};
use crate::storage::{Batch, Column, Schema, Selection, Table, DEFAULT_BATCH_SIZE};

/// Per-operator counters. This is the struct the UI's EXECUTION tab renders,
/// and later the place estimated-vs-actual cardinality lives.
#[derive(Debug, Clone, Default)]
pub struct OperatorStats {
    pub name: String,
    /// One-line description of what this instance does, e.g. the predicate.
    pub detail: String,
    pub rows_in: u64,
    pub rows_out: u64,
    pub batches_out: u64,
    /// Time spent in this operator alone, excluding time spent in children.
    pub elapsed_nanos: u64,
    /// Largest batch this operator produced, in bytes.
    pub peak_batch_bytes: usize,
    // Scan-only.
    pub row_groups_total: u64,
    pub row_groups_scanned: u64,
    pub row_groups_pruned: u64,
    /// Of the pruned groups, how many the bloom filters caught that the zone
    /// map could not. Reported separately because the two structures prune
    /// different shapes of predicate, and seeing which one fired is the whole
    /// point of showing the numbers.
    pub row_groups_bloom_pruned: u64,
    /// Filter-only: how many times a selection got sparse enough to be worth
    /// materializing.
    pub compactions: u64,
    /// What the optimizer predicted this operator would produce.
    ///
    /// Reported next to `rows_out` rather than kept private, because the gap
    /// between the two is the most informative number a query optimizer has --
    /// and the one almost no engine shows you.
    pub estimated_rows: Option<f64>,
}

impl OperatorStats {
    fn new(name: &str, detail: String) -> OperatorStats {
        OperatorStats {
            name: name.to_string(),
            detail,
            ..Default::default()
        }
    }

    fn record_output(&mut self, batch: &Batch) {
        self.rows_out += batch.num_rows() as u64;
        self.batches_out += 1;
        self.peak_batch_bytes = self.peak_batch_bytes.max(batch.byte_size());
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_nanos as f64 / 1_000_000.0
    }

    /// How wrong the estimate was, symmetrically: 10x over is as bad as 10x
    /// under. `None` when nothing was estimated.
    pub fn q_error(&self) -> Option<f64> {
        self.estimated_rows
            .map(|e| crate::optimizer::stats::q_error(e, self.rows_out as f64))
    }
}

/// Wall-clock timer.
///
/// `std::time::Instant` compiles for wasm32-unknown-unknown but panics when
/// called, since that target has no clock. Rather than pull in a wasm
/// dependency here (the engine crate stays dependency-free), timing simply
/// reports zero there; the wasm boundary will inject a real clock.
pub struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
    #[cfg(target_arch = "wasm32")]
    start_millis: f64,
}

/// The clock, on a target that has none of its own.
///
/// `std::time::Instant` panics on `wasm32-unknown-unknown`: there is no clock
/// in the WebAssembly system interface, only whatever the host chooses to hand
/// in. So the boundary crate installs one -- `performance.now()` in a browser --
/// and the engine keeps its promise of having no dependencies and no host
/// assumptions.
///
/// A plain `fn` pointer rather than a boxed closure, so this stays a `OnceLock`
/// of a `Copy` type with no allocation and no unsafe.
#[cfg(target_arch = "wasm32")]
static CLOCK: std::sync::OnceLock<fn() -> f64> = std::sync::OnceLock::new();

/// Install the clock. Called once, before any query runs; later calls are
/// ignored, so a host cannot change time out from under a running query.
#[cfg(target_arch = "wasm32")]
pub fn set_clock(now_millis: fn() -> f64) {
    let _ = CLOCK.set(now_millis);
}

/// Milliseconds from the host clock, or zero if none was installed. Reporting
/// zero is the honest answer for "this host has no clock" -- it makes every
/// operator's share of the time zero rather than inventing a number.
#[cfg(target_arch = "wasm32")]
fn now_millis() -> f64 {
    CLOCK.get().map_or(0.0, |f| f())
}

impl Timer {
    pub fn start() -> Timer {
        Timer {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
            #[cfg(target_arch = "wasm32")]
            start_millis: now_millis(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn elapsed_nanos(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    #[cfg(target_arch = "wasm32")]
    pub fn elapsed_nanos(&self) -> u64 {
        // `performance.now()` is a float of milliseconds, and in a
        // cross-origin-isolated context it is deliberately coarsened -- so
        // sub-microsecond operator timings on wasm are noise, not measurement.
        let elapsed = now_millis() - self.start_millis;
        if elapsed <= 0.0 {
            0
        } else {
            (elapsed * 1_000_000.0) as u64
        }
    }
}

pub trait Operator {
    fn schema(&self) -> Arc<Schema>;

    /// Produce the next batch, or `None` when the operator is exhausted.
    /// A returned batch may be empty only if the operator chooses; the ones
    /// here never return an empty non-final batch.
    fn next(&mut self) -> Result<Option<Batch>>;

    fn stats(&self) -> &OperatorStats;

    fn children(&self) -> Vec<&dyn Operator> {
        Vec::new()
    }

    /// Record the optimizer's prediction for this operator.
    fn set_estimated_rows(&mut self, _rows: f64) {}

    /// Mutable access to child `i`, so estimates can be attached after the tree
    /// is built. `None` for a leaf or an out-of-range index.
    fn child_mut(&mut self, _index: usize) -> Option<&mut dyn Operator> {
        None
    }
}

// ---------------------------------------------------------------------------
// Execution options
// ---------------------------------------------------------------------------

/// Which expression evaluator the operators use.
///
/// `Scalar` exists so the two can be compared: for correctness, by running the
/// same query both ways and demanding identical results, and for performance,
/// by measuring what vectorization actually bought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evaluator {
    Vectorized,
    Scalar,
}

/// Which join implementation the planner is allowed to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinAlgorithm {
    /// Hash join when the condition offers an equality between the two sides,
    /// nested loop otherwise.
    Auto,
    /// Always nested loop. The point is testing: the nested loop compares every
    /// pair against the whole condition, so it is the obvious implementation
    /// that the hash join has to agree with -- the same relationship the scalar
    /// evaluator has with the vectorized one.
    ForceNestedLoop,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub evaluator: Evaluator,
    pub join_algorithm: JoinAlgorithm,
    /// Whether the binder's plan is run through the optimizer. Off is how the
    /// tests check that every rewrite preserves semantics.
    pub optimize: bool,
    /// Whether cost-based join reordering is among the rules applied. Separable
    /// from the rest so its effect can be measured on its own.
    pub reorder_joins: bool,
    /// Whether a `LIMIT` above a `Sort` becomes a bounded top-N heap instead of
    /// ordering the whole input.
    pub top_n: bool,
    /// Whether `IN` and `EXISTS` become semi- and anti-joins.
    ///
    /// Turning this off leaves them to be evaluated and materialized instead,
    /// which is a real alternative for an *uncorrelated* subquery and no
    /// alternative at all for a correlated one -- so a correlated query simply
    /// fails to plan. That asymmetry is the point of measuring it.
    pub decorrelate: bool,
    /// Whether scans consult row-group zone maps to skip groups.
    pub zone_map_pruning: bool,
    /// Whether scans consult per-row-group bloom filters for equality.
    pub bloom_filters: bool,
    /// Whether a selective predicate on an indexed column may be served by a
    /// B+ tree lookup instead of a scan.
    pub index_scans: bool,
    pub batch_size: usize,
    /// Fraction of a batch's physical rows below which a filter compacts
    /// instead of carrying a selection vector. See `Selection::should_compact`.
    pub compact_threshold: f64,
}

impl Default for ExecOptions {
    fn default() -> ExecOptions {
        ExecOptions {
            evaluator: Evaluator::Vectorized,
            join_algorithm: JoinAlgorithm::Auto,
            optimize: true,
            reorder_joins: true,
            top_n: true,
            decorrelate: true,
            zone_map_pruning: true,
            bloom_filters: true,
            index_scans: true,
            batch_size: DEFAULT_BATCH_SIZE,
            // Off, on the evidence. Measured on the 1M-row taxi fixture, total
            // benchmark time rises monotonically with this threshold:
            // 74.9ms at 0.0, 75.2ms at 0.1, 76.2ms at 0.2, 78.0ms at 0.4.
            //
            // That spread was 88ms to 122ms when first measured, before
            // projection pushdown landed. The filter used to compact all eight
            // columns of the row group and now compacts the one or two the
            // query reads, so the penalty fell from 39% to 4% -- the same
            // verdict, on a much narrower margin.
            //
            // The reason is the shape of today's plans. The only operator
            // reading through a filter's selection is the Project above it, and
            // a Project materializes anyway -- so compacting first is a second
            // copy, made worse by the fact that the filter compacts every
            // column while the project usually needs one or two.
            //
            // Compaction earns its keep once a sparse selection is read by
            // several operators in a row -- filter over filter, a join probe, a
            // hash aggregate. Revisit this constant then, with those plans in
            // the benchmark set; the mechanism is here and measured, it just
            // has nothing to pay for yet.
            compact_threshold: 0.0,
        }
    }
}

impl ExecOptions {
    pub fn scalar() -> ExecOptions {
        ExecOptions {
            evaluator: Evaluator::Scalar,
            ..Default::default()
        }
    }

    pub fn nested_loop_joins() -> ExecOptions {
        ExecOptions {
            join_algorithm: JoinAlgorithm::ForceNestedLoop,
            ..Default::default()
        }
    }

    /// The plan exactly as the binder produced it. Every rewrite has to agree
    /// with this, which is the whole definition of "semantics-preserving".
    pub fn unoptimized() -> ExecOptions {
        ExecOptions {
            optimize: false,
            ..Default::default()
        }
    }

    /// No metadata pruning at all: every row group is read. The baseline the
    /// pruning configurations must agree with.
    pub fn without_pruning() -> ExecOptions {
        ExecOptions {
            zone_map_pruning: false,
            bloom_filters: false,
            ..Default::default()
        }
    }

    pub fn without_bloom_filters() -> ExecOptions {
        ExecOptions {
            bloom_filters: false,
            ..Default::default()
        }
    }

    /// Forces the full-scan path even where an index exists. Every query must
    /// return exactly the same rows in the same order either way -- that
    /// equivalence is what makes the index a physical choice rather than a
    /// change of semantics.
    pub fn without_index_scans() -> ExecOptions {
        ExecOptions {
            index_scans: false,
            ..Default::default()
        }
    }

    /// Every rule except join reordering, so the join order is whatever the
    /// query said.
    pub fn without_join_reorder() -> ExecOptions {
        ExecOptions {
            reorder_joins: false,
            ..Default::default()
        }
    }

    pub fn without_top_n() -> ExecOptions {
        ExecOptions {
            top_n: false,
            ..Default::default()
        }
    }

    pub fn without_decorrelation() -> ExecOptions {
        ExecOptions {
            decorrelate: false,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// OneRow
// ---------------------------------------------------------------------------

/// A single row with no columns, so that a FROM-less `SELECT 1 + 1` evaluates
/// its projection exactly once.
pub struct OneRowExec {
    schema: Arc<Schema>,
    done: bool,
    stats: OperatorStats,
}

impl OneRowExec {
    pub fn new() -> OneRowExec {
        OneRowExec {
            schema: Arc::new(Schema::empty()),
            done: false,
            stats: OperatorStats::new("OneRow", String::new()),
        }
    }
}

impl Default for OneRowExec {
    fn default() -> Self {
        OneRowExec::new()
    }
}

impl Operator for OneRowExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let batch = Batch::empty_rows(Arc::clone(&self.schema), 1);
        self.stats.record_output(&batch);
        Ok(Some(batch))
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

/// Full table scan.
///
/// Emits windows onto the row groups' own columns -- no data is copied. A batch
/// is a shared pointer to each column plus a contiguous `Selection::Range`, so
/// scanning a million rows costs a handful of refcount bumps.
///
/// The loop walks row groups, which is exactly the granularity zone-map pruning
/// will operate at: there is already a per-row-group decision point where
/// `row_groups_pruned` gets incremented.
pub struct ScanExec {
    table: Arc<Table>,
    batch_size: usize,
    /// Which table columns to emit, in order. `None` means all of them.
    projection: Option<Vec<usize>>,
    schema: Arc<Schema>,
    /// A predicate the scan may use to skip whole row groups. It is a *hint*:
    /// the Filter above still evaluates it, so a bug in pruning shows up as
    /// missing rows, never as wrong ones.
    prune: Option<(crate::plan::BoundExpr, RelId)>,
    /// Whether the per-row-group bloom filters are consulted after the zone map
    /// fails to rule a group out.
    blooms: bool,
    zone_maps: bool,
    row_group: usize,
    offset_in_group: usize,
    /// The projected columns of the row group being read.
    ///
    /// Filled once when the scan enters a group rather than per batch, because
    /// for a lazily loaded table getting a column may mean decoding a whole
    /// compressed chunk -- and doing that thirty-two times for a 64K row group
    /// read in 2048-row batches would be absurd.
    group_columns: Vec<Arc<Column>>,
    stats: OperatorStats,
}

impl ScanExec {
    pub fn new(table: Arc<Table>) -> ScanExec {
        let mut stats = OperatorStats::new("Scan", format!("table={}", table.name));
        stats.row_groups_total = table.num_row_groups() as u64;
        let schema = Arc::clone(&table.schema);
        ScanExec {
            table,
            batch_size: DEFAULT_BATCH_SIZE,
            projection: None,
            schema,
            prune: None,
            blooms: true,
            zone_maps: true,
            row_group: 0,
            offset_in_group: 0,
            group_columns: Vec::new(),
            stats,
        }
    }

    pub fn with_batch_size(mut self, n: usize) -> ScanExec {
        self.batch_size = n.max(1);
        self
    }

    pub fn with_projection(
        mut self,
        projection: Option<Vec<usize>>,
        schema: Arc<Schema>,
    ) -> ScanExec {
        if let Some(p) = &projection {
            let pruned = self.table.schema.len() - p.len();
            if pruned > 0 {
                self.stats.detail =
                    format!("{}, {pruned} column(s) pruned", self.stats.detail);
            }
        }
        self.projection = projection;
        self.schema = schema;
        self
    }

    pub fn with_prune_predicate(mut self, predicate: crate::plan::BoundExpr, rel: RelId) -> ScanExec {
        let has_bloom = self
            .table
            .row_groups
            .iter()
            .any(|rg| rg.blooms.iter().any(|b| b.is_some()));
        let structures = if self.blooms && has_bloom {
            "zone maps + blooms"
        } else {
            "zone maps"
        };
        self.stats.detail =
            format!("{}, {structures} on {}", self.stats.detail, predicate.to_sql());
        self.prune = Some((predicate, rel));
        self
    }

    pub fn with_bloom_filters(mut self, on: bool) -> ScanExec {
        self.blooms = on;
        self
    }

    /// Keep the predicate for the bloom filters but stop consulting zone maps.
    /// Only the agreement tests want this: it isolates one structure from the
    /// other so each can be shown to lose no rows on its own.
    pub fn without_zone_maps(mut self) -> ScanExec {
        self.zone_maps = false;
        self
    }

    /// Whether this row group can be skipped without reading any values.
    ///
    /// Two structures, cheapest first. The zone map costs two comparisons on
    /// metadata; the bloom filters cost a handful of hashes. Both are one-sided
    /// in the same direction -- they only ever say "certainly nothing here" --
    /// so consulting the second after the first says nothing can add skips but
    /// can never remove a row.
    fn prunable(&mut self, index: usize) -> bool {
        let Some((predicate, rel)) = &self.prune else {
            return false;
        };
        let rg = &self.table.row_groups[index];
        if self.zone_maps && !prune::row_group_can_match(predicate, *rel, &rg.stats, rg.num_rows) {
            return true;
        }
        if self.blooms && prune::bloom_rejects(predicate, *rel, &rg.blooms, &self.table.schema) {
            self.stats.row_groups_bloom_pruned += 1;
            return true;
        }
        false
    }
}

impl Operator for ScanExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        let timer = Timer::start();
        // A local handle so that `prunable` can record its own counters without
        // the row-group borrow outliving the call.
        let table = Arc::clone(&self.table);
        let result = loop {
            let Some(rg) = table.row_groups.get(self.row_group) else {
                break None;
            };
            if self.offset_in_group == 0 {
                // The zone-map decision is made once per group, on metadata
                // alone -- two comparisons to skip up to 64K rows.
                if self.prunable(self.row_group) {
                    self.stats.row_groups_pruned += 1;
                    self.row_group += 1;
                    continue;
                }
                self.stats.row_groups_scanned += 1;
                // Only now, past the zone maps and the bloom filters, is any
                // data actually read. For a Parquet table a pruned group costs
                // nothing but the two comparisons that ruled it out.
                self.group_columns = match &self.projection {
                    Some(p) => p.iter().map(|i| rg.column(*i)).collect::<Result<_>>()?,
                    None => rg.all_columns()?,
                };
            }
            if self.offset_in_group >= rg.num_rows {
                self.row_group += 1;
                self.offset_in_group = 0;
                continue;
            }
            let len = self.batch_size.min(rg.num_rows - self.offset_in_group);
            let columns: Vec<Arc<Column>> = self.group_columns.clone();
            let selection = Selection::Range {
                offset: self.offset_in_group,
                len,
            };
            self.offset_in_group += len;
            break Some(Batch::new(Arc::clone(&self.schema), columns, selection));
        };

        self.stats.elapsed_nanos += timer.elapsed_nanos();
        if let Some(b) = &result {
            self.stats.rows_in += b.num_rows() as u64;
            self.stats.record_output(b);
        }
        Ok(result)
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

enum Predicate {
    Vectorized(CompiledExpr),
    Scalar {
        expr: crate::plan::BoundExpr,
        ctx: EvalContext,
    },
}

/// Evaluates a predicate and narrows the batch's selection.
///
/// The surviving rows are *not* copied. A filter that keeps 40% of a batch
/// would otherwise rewrite every column to keep 40% of it -- including columns
/// nothing downstream reads. Instead the batch keeps pointing at the same
/// buffers under a narrower selection, and only compacts when the selection
/// gets sparse enough that the gather costs more than the copy.
pub struct FilterExec {
    input: Box<dyn Operator>,
    predicate: Predicate,
    compact_threshold: f64,
    stats: OperatorStats,
}

impl FilterExec {
    pub fn new(
        input: Box<dyn Operator>,
        predicate: crate::plan::BoundExpr,
        ctx: EvalContext,
        options: &ExecOptions,
    ) -> Result<FilterExec> {
        let detail = predicate.to_sql();
        let predicate = match options.evaluator {
            Evaluator::Vectorized => Predicate::Vectorized(expr::compile(&predicate, &ctx)?),
            Evaluator::Scalar => Predicate::Scalar { expr: predicate, ctx },
        };
        Ok(FilterExec {
            input,
            predicate,
            compact_threshold: options.compact_threshold,
            stats: OperatorStats::new("Filter", detail),
        })
    }

    fn evaluate(&self, batch: &Batch) -> Result<Vec<u32>> {
        match &self.predicate {
            Predicate::Vectorized(c) => expr::eval_predicate(c, batch),
            Predicate::Scalar { expr, ctx } => expr::scalar::eval_predicate(expr, batch, ctx),
        }
    }
}

impl Operator for FilterExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        loop {
            // The child's time is not ours: the clock starts after its `next`
            // returns, so per-operator times are exclusive and sum correctly.
            let Some(batch) = self.input.next()? else {
                return Ok(None);
            };
            let timer = Timer::start();
            self.stats.rows_in += batch.num_rows() as u64;

            let keep = self.evaluate(&batch)?;
            let mut out = batch.filter(&keep);
            if out
                .selection()
                .should_compact(out.window(), self.compact_threshold)
            {
                out = out.compact();
                self.stats.compactions += 1;
            }
            self.stats.elapsed_nanos += timer.elapsed_nanos();

            // Don't hand an empty batch upward; pull again instead.
            if out.num_rows() == 0 {
                continue;
            }
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

enum Projection {
    Vectorized(Vec<CompiledExpr>),
    Scalar {
        exprs: Vec<crate::plan::BoundExpr>,
        ctx: EvalContext,
    },
}

pub struct ProjectExec {
    input: Box<dyn Operator>,
    exprs: Projection,
    schema: Arc<Schema>,
    stats: OperatorStats,
}

impl ProjectExec {
    pub fn new(
        input: Box<dyn Operator>,
        exprs: Vec<crate::plan::BoundExpr>,
        schema: Arc<Schema>,
        ctx: EvalContext,
        options: &ExecOptions,
    ) -> Result<ProjectExec> {
        let detail = exprs
            .iter()
            .map(|e| e.to_sql())
            .collect::<Vec<_>>()
            .join(", ");
        let exprs = match options.evaluator {
            Evaluator::Vectorized => Projection::Vectorized(
                exprs
                    .iter()
                    .map(|e| expr::compile(e, &ctx))
                    .collect::<Result<_>>()?,
            ),
            Evaluator::Scalar => Projection::Scalar { exprs, ctx },
        };
        Ok(ProjectExec {
            input,
            exprs,
            schema,
            stats: OperatorStats::new("Project", detail),
        })
    }
}

impl Operator for ProjectExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        let Some(batch) = self.input.next()? else {
            return Ok(None);
        };
        let timer = Timer::start();
        let rows = batch.num_rows();
        self.stats.rows_in += rows as u64;

        // A projection produces new values, so its output is always dense --
        // this is where a selection vector finally gets paid off, and only for
        // the columns that are actually projected.
        let columns: Vec<Arc<Column>> = match &self.exprs {
            Projection::Vectorized(compiled) => compiled
                .iter()
                .map(|e| expr::eval(e, &batch))
                .collect::<Result<_>>()?,
            Projection::Scalar { exprs, ctx } => exprs
                .iter()
                .map(|e| expr::scalar::eval_column(e, &batch, ctx).map(Arc::new))
                .collect::<Result<_>>()?,
        };
        let out = Batch::new(Arc::clone(&self.schema), columns, Selection::full(rows));

        self.stats.elapsed_nanos += timer.elapsed_nanos();
        self.stats.record_output(&out);
        Ok(Some(out))
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}

// ---------------------------------------------------------------------------
// Limit
// ---------------------------------------------------------------------------

pub struct LimitExec {
    input: Box<dyn Operator>,
    skip: usize,
    fetch: Option<usize>,
    seen: usize,
    emitted: usize,
    stats: OperatorStats,
}

impl LimitExec {
    pub fn new(input: Box<dyn Operator>, skip: usize, fetch: Option<usize>) -> LimitExec {
        let detail = match fetch {
            Some(n) => format!("fetch={n} skip={skip}"),
            None => format!("skip={skip}"),
        };
        LimitExec {
            input,
            skip,
            fetch,
            seen: 0,
            emitted: 0,
            stats: OperatorStats::new("Limit", detail),
        }
    }

    fn exhausted(&self) -> bool {
        matches!(self.fetch, Some(n) if self.emitted >= n)
    }
}

impl Operator for LimitExec {

    fn set_estimated_rows(&mut self, rows: f64) {
        self.stats.estimated_rows = Some(rows);
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut dyn Operator> {
        match index {
            0 => Some(self.input.as_mut()),
            _ => None,
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Batch>> {
        loop {
            if self.exhausted() {
                // Stop pulling once the limit is met. With a sort below this,
                // that early stop is what makes top-N cheap.
                return Ok(None);
            }
            let Some(batch) = self.input.next()? else {
                return Ok(None);
            };
            let timer = Timer::start();
            let rows = batch.num_rows();
            self.stats.rows_in += rows as u64;

            // Drop rows still inside the OFFSET window.
            let start = if self.seen < self.skip {
                (self.skip - self.seen).min(rows)
            } else {
                0
            };
            self.seen += rows;

            let mut available = rows - start;
            if let Some(n) = self.fetch {
                available = available.min(n - self.emitted);
            }

            if available == 0 {
                self.stats.elapsed_nanos += timer.elapsed_nanos();
                continue;
            }
            let out = if start == 0 && available == rows {
                batch
            } else {
                batch.slice(start, available)
            };
            self.emitted += available;

            self.stats.elapsed_nanos += timer.elapsed_nanos();
            self.stats.record_output(&out);
            return Ok(Some(out));
        }
    }

    fn stats(&self) -> &OperatorStats {
        &self.stats
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![self.input.as_ref()]
    }
}

// ---------------------------------------------------------------------------
// Building an operator tree
// ---------------------------------------------------------------------------

/// Turn a logical plan into operators.
///
/// This is not yet a physical planner: there is exactly one implementation per
/// logical node, so there is nothing to choose between. It is the seam where
/// operator selection (hash join vs merge join, hash aggregate vs sort
/// aggregate, full scan vs index scan) will go.
pub fn build(plan: &LogicalPlan, catalog: &Catalog) -> Result<Box<dyn Operator>> {
    build_with(plan, catalog, &ExecOptions::default())
}

pub fn build_with(
    plan: &LogicalPlan,
    catalog: &Catalog,
    options: &ExecOptions,
) -> Result<Box<dyn Operator>> {
    // Estimated cardinalities are attached as the tree is built, so that after
    // execution every operator can report its prediction beside the truth.
    let estimates = crate::optimizer::stats::estimate_all(plan, catalog);
    let mut root = build_node(plan, catalog, options, &estimates)?;
    annotate(root.as_mut(), plan, &estimates);
    Ok(root)
}

/// Attach each node's estimate to its stats, walking plan and operator tree in
/// step. They have the same shape by construction.
fn annotate(
    op: &mut dyn Operator,
    plan: &LogicalPlan,
    estimates: &std::collections::HashMap<RelId, f64>,
) {
    // A subquery alias renames, it does not execute -- so it has no operator
    // and the two trees would otherwise drift a level apart from here down,
    // attaching every estimate to the wrong operator.
    if let LogicalPlan::SubqueryAlias { input, .. } = plan {
        return annotate(op, input, estimates);
    }
    if let Some(rows) = estimates.get(&plan.rel()) {
        op.set_estimated_rows(*rows);
    }
    let children = plan.children();
    for (i, child) in children.iter().enumerate() {
        if let Some(op_child) = op.child_mut(i) {
            annotate(op_child, child, estimates);
        }
    }
}

/// Try to serve a filtered scan from a B+ tree index.
///
/// Returns `None` -- meaning "scan instead" -- whenever the index cannot help:
/// no index on a column the predicate constrains, no bound the tree can use,
/// or too many rows qualifying for a gather to beat a sequential pass. That
/// last case is decided on the *true* matching row count, not an estimate:
/// `range_limited` walks the leaves and gives up at the cap, so a bad guess
/// costs a bounded amount of work rather than a bad plan.
///
/// When several indexes apply, the one naming the fewest rows wins. Comparing
/// them is nearly free once each lookup has been capped.
fn index_scan(
    table: &Arc<Table>,
    catalog: &Catalog,
    predicate: &crate::plan::BoundExpr,
    rel: RelId,
    projection: &Option<Vec<usize>>,
    schema: &Arc<Schema>,
    options: &ExecOptions,
) -> Option<Box<dyn Operator>> {
    let cap = index::index_row_cap(table.num_rows());
    let mut best: Option<(Vec<u32>, &Arc<crate::catalog::TableIndex>, IndexRange)> = None;

    for idx in catalog.indexes_on(&table.name) {
        let class = crate::types::value_class(&table.schema.field(idx.column).data_type);
        let Some(range) = index::extract_range(predicate, rel, idx.column, class) else {
            continue;
        };
        let Some(rows) = idx.tree.range_limited(&range.lower, &range.upper, cap) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _, _)| rows.len() < b.len()) {
            best = Some((rows, idx, range));
        }
    }

    let (rows, idx, range) = best?;
    Some(Box::new(
        IndexScanExec::new(Arc::clone(table), rows, &idx.column_name, &range)
            .with_batch_size(options.batch_size)
            .with_projection(projection.clone(), Arc::clone(schema)),
    ))
}

fn build_node(
    plan: &LogicalPlan,
    catalog: &Catalog,
    options: &ExecOptions,
    estimates: &std::collections::HashMap<RelId, f64>,
) -> Result<Box<dyn Operator>> {
    let _ = estimates;
    Ok(match plan {
        LogicalPlan::OneRow { .. } => Box::new(OneRowExec::new()),

        LogicalPlan::Scan { table_name, projection, schema, .. } => {
            let table = catalog.get(table_name, false).ok_or_else(|| {
                Diagnostic::exec(format!(
                    "table `{table_name}` disappeared from the catalog between planning and execution"
                ))
            })?;
            Box::new(
                ScanExec::new(table)
                    .with_batch_size(options.batch_size)
                    .with_projection(projection.clone(), Arc::clone(schema)),
            )
        }

        LogicalPlan::Filter { predicate, input, .. } => {
            let ctx = output_context(input);
            // A filter sitting directly on a scan hands its predicate down.
            // What the scan does with it is the widest physical choice in the
            // engine -- index lookup, pruned scan, or full scan -- and all
            // three are safe for the same reason: the filter above still
            // evaluates the predicate, and none of them can do more than
            // decide which rows are *read*.
            let child = match &**input {
                LogicalPlan::Scan { rel, projection, schema, table_name, .. } => {
                    let table = catalog.get(table_name, false).ok_or_else(|| {
                        Diagnostic::exec(format!("table `{table_name}` disappeared from the catalog"))
                    })?;
                    let indexed = if options.index_scans {
                        index_scan(&table, catalog, predicate, *rel, projection, schema, options)
                    } else {
                        None
                    };
                    match indexed {
                        Some(op) => op,
                        None => {
                            let mut scan = ScanExec::new(table)
                                .with_batch_size(options.batch_size)
                                .with_projection(projection.clone(), Arc::clone(schema))
                                .with_bloom_filters(options.bloom_filters);
                            if options.zone_map_pruning || options.bloom_filters {
                                scan = scan.with_prune_predicate(predicate.clone(), *rel);
                            }
                            if !options.zone_map_pruning {
                                scan = scan.without_zone_maps();
                            }
                            Box::new(scan) as Box<dyn Operator>
                        }
                    }
                }
                _ => build_node(input, catalog, options, estimates)?,
            };
            Box::new(FilterExec::new(child, predicate.clone(), ctx, options)?)
        }

        LogicalPlan::Project { exprs, schema, input, .. } => {
            let ctx = output_context(input);
            Box::new(ProjectExec::new(
                build_node(input, catalog, options, estimates)?,
                exprs.clone(),
                Arc::clone(schema),
                ctx,
                options,
            )?)
        }

        LogicalPlan::Limit { skip, fetch, input, .. } => {
            // A limit directly above a sort is a top-N: only the first
            // `skip + fetch` rows can matter, so a bounded heap replaces
            // ordering the whole input. This is the second real operator
            // choice, after hash versus nested loop.
            if let (true, Some(fetch), LogicalPlan::Sort { keys, input: sorted, .. }) =
                (options.top_n, fetch, &**input)
            {
                let ctx = output_context(sorted);
                let compiled = keys
                    .iter()
                    .map(|k| {
                        Ok(CompiledSortKey {
                            expr: expr::compile(&k.expr, &ctx)?,
                            ascending: k.ascending,
                            nulls_first: k.nulls_first,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let schema = sorted.schema();
                let top = TopNExec::new(
                    build_node(sorted, catalog, options, estimates)?,
                    compiled,
                    schema,
                    skip + fetch,
                );
                return Ok(Box::new(LimitExec::new(Box::new(top), *skip, Some(*fetch))));
            }
            Box::new(LimitExec::new(
                build_node(input, catalog, options, estimates)?,
                *skip,
                *fetch,
            ))
        }

        LogicalPlan::Sort { keys, input, .. } => {
            let ctx = output_context(input);
            let compiled = keys
                .iter()
                .map(|k| {
                    Ok(CompiledSortKey {
                        expr: expr::compile(&k.expr, &ctx)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let schema = input.schema();
            Box::new(SortExec::new(
                build_node(input, catalog, options, estimates)?,
                compiled,
                schema,
            ))
        }

        LogicalPlan::Distinct { input, .. } => Box::new(DistinctExec::new(build_node(
            input, catalog, options, estimates,
        )?)),

        LogicalPlan::Window { partition_by, order_by, functions, schema, input, .. } => {
            let ctx = output_context(input);
            let partition = partition_by
                .iter()
                .map(|e| {
                    Ok(CompiledSortKey {
                        expr: expr::compile(e, &ctx)?,
                        ascending: true,
                        nulls_first: true,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let order = order_by
                .iter()
                .map(|k| {
                    Ok(CompiledSortKey {
                        expr: expr::compile(&k.expr, &ctx)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let compiled = functions
                .iter()
                .map(|f| {
                    Ok(CompiledWindowFunction {
                        func: f.func,
                        args: f
                            .args
                            .iter()
                            .map(|a| expr::compile(a, &ctx))
                            .collect::<Result<Vec<_>>>()?,
                        frame: f.frame,
                        data_type: f.data_type,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Box::new(WindowExec::new(
                build_node(input, catalog, options, estimates)?,
                partition,
                order,
                compiled,
                Arc::clone(schema),
            ))
        }

        LogicalPlan::SetOp { op, all, left, right, schema, .. } => Box::new(SetOpExec::new(
            build_node(left, catalog, options, estimates)?,
            build_node(right, catalog, options, estimates)?,
            *op,
            *all,
            Arc::clone(schema),
        )),

        LogicalPlan::Join { join_type, on, left, right, schema, .. } => {
            build_join(*join_type, on.as_ref(), left, right, schema, catalog, options, estimates)?
        }

        // Naming a subquery's output is a planning concern only -- there is
        // nothing to do at run time but hand the rows through.
        LogicalPlan::SubqueryAlias { input, .. } => build_node(input, catalog, options, estimates)?,

        LogicalPlan::Aggregate { group_exprs, aggregates, schema, input, .. } => {
            let ctx = output_context(input);
            let compiled_groups = group_exprs
                .iter()
                .map(|e| expr::compile(e, &ctx))
                .collect::<Result<Vec<_>>>()?;
            let compiled_aggs = aggregates
                .iter()
                .map(|a| {
                    Ok(CompiledAggregate {
                        func: a.func,
                        arg: match &a.arg {
                            Some(e) => Some(expr::compile(e, &ctx)?),
                            None => None,
                        },
                        distinct: a.distinct,
                        data_type: a.data_type,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Box::new(HashAggregateExec::new(
                build_node(input, catalog, options, estimates)?,
                compiled_groups,
                compiled_aggs,
                Arc::clone(schema),
            ))
        }
    })
}

/// Choose a join implementation.
///
/// This is the engine's first real operator selection. The rule is simple and
/// entirely structural: a hash join needs something to hash on, so if the
/// condition yields at least one equality between the two sides, it is used;
/// otherwise the only thing left is to compare every pair.
///
/// What is *not* decided here is which side to build. You want the smaller one,
/// and knowing which that is takes cardinality estimates, so for now the right
/// input is always built. A merge join -- the third choice, for inputs that
/// arrive sorted -- has no way to exist until there is a Sort operator to
/// produce sorted inputs or a plan property to notice they already are.
#[allow(clippy::too_many_arguments)]
fn build_join(
    join_type: JoinType,
    on: Option<&BoundExpr>,
    left: &LogicalPlan,
    right: &LogicalPlan,
    schema: &Arc<Schema>,
    catalog: &Catalog,
    options: &ExecOptions,
    estimates: &std::collections::HashMap<RelId, f64>,
) -> Result<Box<dyn Operator>> {
    let left_ctx = output_context(left);
    let right_ctx = output_context(right);
    let joined_ctx = EvalContext::concat(&left_ctx, &right_ctx);

    let left_rels = relations_of(&left_ctx);
    let right_rels = relations_of(&right_ctx);
    let (equi, residual) = match (on, options.join_algorithm) {
        // Forcing a nested loop means handing it the whole condition rather
        // than splitting anything out as a hash key.
        (Some(c), JoinAlgorithm::ForceNestedLoop) => (Vec::new(), Some(c.clone())),
        (Some(c), JoinAlgorithm::Auto) => split_join_condition(c.clone(), &left_rels, &right_rels),
        (None, _) => (Vec::new(), None),
    };

    let left_op = build_node(left, catalog, options, estimates)?;
    let right_op = build_node(right, catalog, options, estimates)?;
    let residual = match residual {
        Some(r) => Some(expr::compile(&r, &joined_ctx)?),
        None => None,
    };

    // A semi or anti join emits only the left columns, but its condition still
    // reads both sides, so the operator needs the wider schema too.
    let candidate_schema = if join_type.is_filtering() {
        crate::plan::join_schema(JoinType::Inner, left, right)
    } else {
        Arc::clone(schema)
    };

    if equi.is_empty() {
        return Ok(Box::new(NestedLoopJoinExec::new(
            left_op,
            right_op,
            join_type,
            residual,
            Arc::clone(schema),
            candidate_schema,
        )));
    }

    let mut probe_keys = Vec::with_capacity(equi.len());
    let mut build_keys = Vec::with_capacity(equi.len());
    for (l, r) in &equi {
        probe_keys.push(expr::compile(l, &left_ctx)?);
        build_keys.push(expr::compile(r, &right_ctx)?);
    }

    Ok(Box::new(HashJoinExec::new(
        left_op,
        right_op,
        join_type,
        probe_keys,
        build_keys,
        residual,
        Arc::clone(schema),
        candidate_schema,
    )))
}

fn relations_of(ctx: &EvalContext) -> BTreeSet<RelId> {
    ctx.relations().collect()
}

/// Split a join condition into equi-join key pairs and everything else.
///
/// A conjunct is a key pair when it is an equality whose two halves read from
/// disjoint sides of the join. Everything else -- inequalities, conditions
/// touching only one side, anything spanning both in a shape a hash cannot
/// exploit -- becomes the residual, applied to candidate rows after pairing.
///
/// The residual is *not* the same as a WHERE filter on the output. For an outer
/// join, a row whose only candidate fails the residual must still be emitted,
/// NULL-padded; that is why the join operator applies it rather than a Filter
/// sitting above.
fn split_join_condition(
    on: BoundExpr,
    left_rels: &BTreeSet<RelId>,
    right_rels: &BTreeSet<RelId>,
) -> (Vec<(BoundExpr, BoundExpr)>, Option<BoundExpr>) {
    let mut conjuncts = Vec::new();
    on.split_conjuncts(&mut conjuncts);

    let mut equi = Vec::new();
    let mut residual: Vec<BoundExpr> = Vec::new();

    for c in conjuncts {
        let BoundExprKind::Binary { op: BinaryOperator::Eq, left, right } = &c.kind else {
            residual.push(c);
            continue;
        };
        let lr = left.relation_set();
        let rr = right.relation_set();
        let subset = |s: &BTreeSet<RelId>, of: &BTreeSet<RelId>| !s.is_empty() && s.is_subset(of);

        if subset(&lr, left_rels) && subset(&rr, right_rels) {
            equi.push((*left.clone(), *right.clone()));
        } else if subset(&lr, right_rels) && subset(&rr, left_rels) {
            // Written the other way round; the operator wants (probe, build).
            equi.push((*right.clone(), *left.clone()));
        } else {
            residual.push(c);
        }
    }

    let residual = residual.into_iter().reduce(|a, b| BoundExpr {
        id: a.id,
        data_type: crate::types::DataType::Boolean,
        nullable: a.nullable || b.nullable,
        span: a.span.merge(b.span),
        kind: BoundExprKind::Binary {
            op: BinaryOperator::And,
            left: Box::new(a),
            right: Box::new(b),
        },
    });
    (equi, residual)
}

/// Where each relation's columns start in the batches this plan emits.
///
/// A Project produces new anonymous columns, so nothing above it can address
/// columns by `RelId` -- hence the empty context.
fn output_context(plan: &LogicalPlan) -> EvalContext {
    match plan {
        LogicalPlan::OneRow { .. } => EvalContext::empty(),
        // A projection's columns belong to the projection. Nothing below can
        // see them, but an ORDER BY or a set operation above can.
        LogicalPlan::Project { rel, schema, .. } => EvalContext::identity(*rel, schema.len()),
        LogicalPlan::SetOp { rel, schema, .. } => EvalContext::identity(*rel, schema.len()),
        LogicalPlan::Sort { input, .. } | LogicalPlan::Distinct { input, .. } => {
            output_context(input)
        }
        // A window keeps its input's columns and appends its own.
        LogicalPlan::Window { rel, functions, input, .. } => EvalContext::concat(
            &output_context(input),
            &EvalContext::identity(*rel, functions.len()),
        ),
        LogicalPlan::Scan { rel, table_schema, projection, .. } => match projection {
            Some(p) => EvalContext::projected(*rel, table_schema.len(), p),
            None => EvalContext::identity(*rel, table_schema.len()),
        },
        // An aggregate's output is a new relation in its own right: the SELECT
        // list above it reads grouping keys and aggregate results by index.
        LogicalPlan::Aggregate { rel, schema, .. } => EvalContext::identity(*rel, schema.len()),
        // A derived table's columns belong to the alias, in projection order.
        LogicalPlan::SubqueryAlias { rel, schema, .. } => {
            EvalContext::identity(*rel, schema.len())
        }
        LogicalPlan::Filter { input, .. } | LogicalPlan::Limit { input, .. } => {
            output_context(input)
        }
        LogicalPlan::Join { join_type, left, right, .. } => {
            // A semi or anti join emits the left side alone, so the right
            // contributes no columns *and no width*. Including it would leave
            // anything stacked above -- a window's column offsets, an outer
            // join's right half -- pointing past the end of the batch.
            if join_type.is_filtering() {
                output_context(left)
            } else {
                EvalContext::concat(&output_context(left), &output_context(right))
            }
        }
    }
}

/// Drain an operator to completion.
pub fn collect(op: &mut dyn Operator) -> Result<Vec<Batch>> {
    let mut out = Vec::new();
    while let Some(b) = op.next()? {
        out.push(b);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Stats reporting
// ---------------------------------------------------------------------------

/// A snapshot of the operator tree's counters, detached from the operators
/// themselves so it can outlive them (and, later, be serialized to the UI).
#[derive(Debug, Clone)]
pub struct StatsNode {
    pub stats: OperatorStats,
    pub children: Vec<StatsNode>,
}

pub fn snapshot_stats(op: &dyn Operator) -> StatsNode {
    StatsNode {
        stats: op.stats().clone(),
        children: op.children().into_iter().map(snapshot_stats).collect(),
    }
}

impl StatsNode {
    pub fn total_nanos(&self) -> u64 {
        self.stats.elapsed_nanos + self.children.iter().map(|c| c.total_nanos()).sum::<u64>()
    }
}

/// Render the operator tree with its counters. Time is shown as a percentage
/// of the total so the expensive operator is obvious at a glance.
pub fn explain_stats(root: &StatsNode) -> String {
    let total = root.total_nanos().max(1);
    let mut out = String::new();
    write_stats(&mut out, root, 0, total);
    out
}

fn write_stats(out: &mut String, node: &StatsNode, depth: usize, total: u64) {
    use std::fmt::Write as _;
    for _ in 0..depth {
        out.push_str("  ");
    }
    if depth > 0 {
        out.push_str("-> ");
    }
    let s = &node.stats;
    let pct = 100.0 * s.elapsed_nanos as f64 / total as f64;
    let _ = write!(
        out,
        "{}  rows={} time={:.3}ms ({:.1}%)",
        s.name,
        s.rows_out,
        s.elapsed_ms(),
        pct
    );
    if let (Some(estimated), Some(q)) = (s.estimated_rows, s.q_error()) {
        // Flag an estimate that is off by more than 10x. That threshold is
        // where a wrong estimate starts changing which plan the optimizer would
        // have picked, rather than merely being imprecise.
        let _ = write!(
            out,
            "  est={:.0} q-error={q:.1}x{}",
            estimated,
            if q >= 10.0 { "  <<<" } else { "" }
        );
    }
    if s.row_groups_total > 0 {
        let _ = write!(
            out,
            " row_groups={}/{} scanned",
            s.row_groups_scanned, s.row_groups_total
        );
    }
    if !s.detail.is_empty() {
        let _ = write!(out, "\n{}     {}", "  ".repeat(depth), s.detail);
    }
    out.push('\n');
    for c in &node.children {
        write_stats(out, c, depth + 1, total);
    }
}
