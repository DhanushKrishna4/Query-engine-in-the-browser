//! The WebAssembly boundary.
//!
//! Everything wasm-specific lives here so that `engine` keeps its promise: no
//! dependencies, no host assumptions, and a native CLI that stays the primary
//! development surface. This crate is the only place that knows JavaScript
//! exists.
//!
//! ## Results do not cross the boundary as values
//!
//! Handing a million rows to JavaScript one `ScalarValue` at a time would cost
//! more than running the query. Instead a [`QueryOutcome`] keeps its columns in
//! wasm memory as dense typed buffers and hands out *pointers*; the page builds
//! `Int32Array` / `Float64Array` / `BigInt64Array` views straight onto the
//! module's memory and reads them without a copy.
//!
//! The one rule that comes with that: a view is only valid until wasm allocates
//! again, because growing the memory detaches every existing `ArrayBuffer`. The
//! page re-reads `memory.buffer` for each column and never keeps a view across
//! a call back into the engine.
//!
//! ## Everything else crosses as JSON
//!
//! Schemas, plans, optimizer traces and operator statistics are small,
//! structural, and read once per query. `serde_json` is the right tool and the
//! reason `serde` is in the allowed dependency list -- it is used for the UI's
//! benefit, never on the data path.

use std::sync::Arc;

use engine::exec::StatsNode;
use engine::storage::CsvOptions;
use engine::types::{DataType, ScalarValue};
use engine::{Engine, QueryResult};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Install the panic hook and the clock. Idempotent; the page calls it once.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    // Natively there is nothing to install: `std::time::Instant` works, and
    // this crate is compiled for the host only so its conversions can be tested
    // without a browser.
    #[cfg(target_arch = "wasm32")]
    engine::exec::set_clock(now_millis);
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// `performance` and its `now`, looked up once.
    ///
    /// `Timer::start` runs around every operator's every batch, so three
    /// property lookups per call would put JavaScript reflection on the hot
    /// path of the thing being measured.
    static PERFORMANCE: Option<(JsValue, js_sys::Function)> = {
        // Reached through the global rather than through `window`, so the same
        // code works inside a worker -- which is where a long query belongs.
        let global = js_sys::global();
        let perf = js_sys::Reflect::get(&global, &JsValue::from_str("performance")).ok()?;
        let now = js_sys::Reflect::get(&perf, &JsValue::from_str("now")).ok()?;
        Some((perf, now.dyn_into::<js_sys::Function>().ok()?))
    };
}

/// Milliseconds from `performance.now()`, or zero if this host has none.
#[cfg(target_arch = "wasm32")]
fn now_millis() -> f64 {
    PERFORMANCE.with(|p| {
        let Some((perf, now)) = p else { return 0.0 };
        // Called with `performance` as the receiver. A browser rejects
        // `now.call(null)` outright -- the method is not free-standing -- and
        // the resulting error surfaced as every timing being exactly zero.
        now.call0(perf).ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
    })
}

/// How a column's values are laid out for the page to read.
///
/// The numbers are part of the boundary's contract: the JavaScript side
/// switches on them to pick a typed-array constructor.
#[wasm_bindgen]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    /// No values at all; every row is NULL.
    Null = 0,
    /// One bit per row, least significant first.
    Boolean = 1,
    Int32 = 2,
    /// Read as a `BigInt64Array`.
    Int64 = 3,
    Float64 = 4,
    /// A byte buffer plus `u32` offsets, decoded with `TextDecoder`.
    Utf8 = 5,
    /// Days since 1970-01-01, as `Int32Array`.
    Date32 = 6,
    /// Microseconds since the epoch, as `BigInt64Array`.
    Timestamp = 7,
}

/// A column's values, held in a buffer of their own element type.
///
/// Not a `Vec<u8>` of little-endian bytes, which is what this was first and
/// which is wrong in a way that hides: `Vec<u8>` is aligned to 1, and
/// `new Int32Array(buffer, ptr, n)` throws unless `ptr` is a multiple of 4.
/// Whether it threw depended on where the allocator happened to put the
/// buffer, so it worked in the app and failed in the benchmark. A typed `Vec`
/// carries its element's alignment by construction.
#[derive(Debug)]
enum Buffer {
    /// Boolean bitmaps and UTF-8 bytes, both of which are read as `Uint8Array`.
    Bytes(Vec<u8>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F64(Vec<f64>),
}

impl Buffer {
    fn ptr(&self) -> *const u8 {
        match self {
            Buffer::Bytes(v) => v.as_ptr(),
            Buffer::I32(v) => v.as_ptr() as *const u8,
            Buffer::I64(v) => v.as_ptr() as *const u8,
            Buffer::F64(v) => v.as_ptr() as *const u8,
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Buffer::Bytes(v) => v.len(),
            Buffer::I32(v) => v.len() * 4,
            Buffer::I64(v) => v.len() * 8,
            Buffer::F64(v) => v.len() * 8,
        }
    }
}

/// One output column, dense and contiguous.
#[derive(Debug)]
struct OutColumn {
    kind: ColumnKind,
    /// The values, in the layout `kind` describes.
    values: Buffer,
    /// Offsets into `values`, for `Utf8` only. `len + 1` entries.
    offsets: Vec<u32>,
    /// One bit per row, set where the row is not NULL. Empty when no row is.
    validity: Vec<u8>,
    len: usize,
}

#[derive(Serialize)]
struct FieldInfo {
    name: String,
    #[serde(rename = "type")]
    data_type: String,
    nullable: bool,
    kind: u8,
}

#[derive(Serialize)]
struct StatsInfo {
    name: String,
    detail: String,
    rows_in: u64,
    rows_out: u64,
    batches: u64,
    elapsed_ms: f64,
    peak_batch_bytes: usize,
    estimated_rows: Option<f64>,
    q_error: Option<f64>,
    row_groups_total: u64,
    row_groups_scanned: u64,
    row_groups_pruned: u64,
    row_groups_bloom_pruned: u64,
    compactions: u64,
    children: Vec<StatsInfo>,
}

fn stats_info(node: &StatsNode) -> StatsInfo {
    let s = &node.stats;
    // The gap between prediction and reality, which is the most informative
    // number an optimizer has and the one almost nothing shows you.
    let q_error = s.estimated_rows.map(|est| {
        let actual = s.rows_out as f64;
        let (a, b) = (est.max(1.0), actual.max(1.0));
        (a / b).max(b / a)
    });
    StatsInfo {
        name: s.name.clone(),
        detail: s.detail.clone(),
        rows_in: s.rows_in,
        rows_out: s.rows_out,
        batches: s.batches_out,
        elapsed_ms: s.elapsed_nanos as f64 / 1e6,
        peak_batch_bytes: s.peak_batch_bytes,
        estimated_rows: s.estimated_rows,
        q_error,
        row_groups_total: s.row_groups_total,
        row_groups_scanned: s.row_groups_scanned,
        row_groups_pruned: s.row_groups_pruned,
        row_groups_bloom_pruned: s.row_groups_bloom_pruned,
        compactions: s.compactions,
        children: node.children.iter().map(stats_info).collect(),
    }
}

#[derive(Serialize)]
struct OutcomeMeta {
    columns: Vec<FieldInfo>,
    num_rows: usize,
    elapsed_ms: f64,
    truncated: bool,
    stats: StatsInfo,
}

/// A finished query: its columns, held in wasm memory for the page to read.
#[wasm_bindgen]
#[derive(Debug)]
pub struct QueryOutcome {
    columns: Vec<OutColumn>,
    meta: String,
}

#[wasm_bindgen]
impl QueryOutcome {
    /// Everything about the result except the values: schema, row count,
    /// timing, and the per-operator statistics tree.
    #[wasm_bindgen(getter)]
    pub fn meta(&self) -> String {
        self.meta.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    pub fn kind(&self, column: usize) -> ColumnKind {
        self.columns.get(column).map_or(ColumnKind::Null, |c| c.kind)
    }

    pub fn len(&self, column: usize) -> usize {
        self.columns.get(column).map_or(0, |c| c.len)
    }

    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(|c| c.len == 0)
    }

    /// Pointer to the column's value buffer, for a typed-array view.
    ///
    /// Valid only until the next call into this module: allocating can grow
    /// wasm memory, and growing detaches every `ArrayBuffer` built on it.
    #[wasm_bindgen(js_name = valuesPtr)]
    pub fn values_ptr(&self, column: usize) -> *const u8 {
        self.columns[column].values.ptr()
    }

    #[wasm_bindgen(js_name = valuesBytes)]
    pub fn values_bytes(&self, column: usize) -> usize {
        self.columns[column].values.byte_len()
    }

    #[wasm_bindgen(js_name = offsetsPtr)]
    pub fn offsets_ptr(&self, column: usize) -> *const u32 {
        self.columns[column].offsets.as_ptr()
    }

    /// Pointer to the validity bitmap, or null when no row in the column is
    /// NULL -- which lets the page skip the per-row check entirely.
    #[wasm_bindgen(js_name = validityPtr)]
    pub fn validity_ptr(&self, column: usize) -> *const u8 {
        let v = &self.columns[column].validity;
        if v.is_empty() {
            std::ptr::null()
        } else {
            v.as_ptr()
        }
    }
}

/// Rows above this are kept in wasm and not handed over.
///
/// A grid cannot show a million rows and a browser should not be asked to build
/// the DOM for them. The engine still *computes* every row -- the count and the
/// timings are of the whole query -- and the page says how many it is showing.
const MAX_ROWS_RETURNED: usize = 10_000;

/// Flatten a query's batches into one dense buffer per column.
///
/// A `Batch` may carry a selection vector, and the batches are separate
/// allocations, so this is the one copy the boundary makes. After it, reading
/// is free.
fn build_outcome(result: &QueryResult) -> QueryOutcome {
    let total = result.num_rows();
    let taken = total.min(MAX_ROWS_RETURNED);
    let width = result.schema.len();

    let mut columns: Vec<OutColumn> = (0..width)
        .map(|i| {
            let kind = kind_of(&result.schema.field(i).data_type);
            OutColumn {
                kind,
                values: match kind {
                    ColumnKind::Int32 | ColumnKind::Date32 => Buffer::I32(Vec::new()),
                    ColumnKind::Int64 | ColumnKind::Timestamp => Buffer::I64(Vec::new()),
                    ColumnKind::Float64 => Buffer::F64(Vec::new()),
                    _ => Buffer::Bytes(Vec::new()),
                },
                offsets: if kind == ColumnKind::Utf8 {
                    vec![0]
                } else {
                    Vec::new()
                },
                validity: Vec::new(),
                len: 0,
            }
        })
        .collect();

    let mut any_null = vec![false; width];
    let mut emitted = 0usize;
    'outer: for batch in &result.batches {
        for row in 0..batch.num_rows() {
            if emitted == taken {
                break 'outer;
            }
            for (c, column) in columns.iter_mut().enumerate() {
                let value = batch.value(c, row);
                push_value(column, &value);
                let valid = !value.is_null();
                if !valid {
                    any_null[c] = true;
                }
                set_bit(&mut column.validity, emitted, valid);
                column.len += 1;
            }
            emitted += 1;
        }
    }

    // A column with no NULLs hands back a null pointer instead of a bitmap of
    // all ones, so the page's fast path is the common one.
    for (c, column) in columns.iter_mut().enumerate() {
        if !any_null[c] {
            column.validity.clear();
        }
    }

    let meta = OutcomeMeta {
        columns: (0..width)
            .map(|i| {
                let f = result.schema.field(i);
                FieldInfo {
                    name: f.name.clone(),
                    data_type: f.data_type.to_string(),
                    nullable: f.nullable,
                    kind: columns[i].kind as u8,
                }
            })
            .collect(),
        num_rows: total,
        elapsed_ms: result.elapsed_ms(),
        truncated: total > taken,
        stats: stats_info(&result.stats),
    };

    QueryOutcome {
        columns,
        meta: serde_json::to_string(&meta).unwrap_or_else(|_| "{}".into()),
    }
}

fn kind_of(t: &DataType) -> ColumnKind {
    match t {
        DataType::Null => ColumnKind::Null,
        DataType::Boolean => ColumnKind::Boolean,
        DataType::Int32 => ColumnKind::Int32,
        DataType::Int64 => ColumnKind::Int64,
        DataType::Float64 => ColumnKind::Float64,
        DataType::Date32 => ColumnKind::Date32,
        DataType::Timestamp => ColumnKind::Timestamp,
        // A decimal has no typed array to land in -- JavaScript has no 128-bit
        // integer and its `number` would lose the exactness the type exists
        // for. Rendering it here keeps the precision the engine computed.
        DataType::Utf8 | DataType::Decimal128 { .. } => ColumnKind::Utf8,
    }
}

fn push_value(column: &mut OutColumn, value: &ScalarValue) {
    let row = column.len;
    match (&mut column.values, column.kind) {
        (Buffer::Bytes(bytes), ColumnKind::Boolean) => {
            let byte = row / 8;
            if bytes.len() <= byte {
                bytes.push(0);
            }
            if matches!(value, ScalarValue::Boolean(true)) {
                bytes[byte] |= 1 << (row % 8);
            }
        }
        (Buffer::Bytes(bytes), ColumnKind::Utf8) => {
            // NULL is stored as an empty slice; the validity bitmap is what
            // distinguishes it from an empty string.
            if !value.is_null() {
                match value {
                    ScalarValue::Utf8(s) => bytes.extend_from_slice(s.as_bytes()),
                    other => bytes.extend_from_slice(other.to_string().as_bytes()),
                }
            }
            column.offsets.push(bytes.len() as u32);
        }
        (Buffer::I32(v), _) => v.push(match value {
            ScalarValue::Int32(x) => *x,
            ScalarValue::Date32(x) => *x,
            _ => 0,
        }),
        (Buffer::I64(v), _) => v.push(match value {
            ScalarValue::Int64(x) => *x,
            ScalarValue::Timestamp(x) => *x,
            ScalarValue::Int32(x) => *x as i64,
            _ => 0,
        }),
        (Buffer::F64(v), _) => v.push(match value {
            ScalarValue::Float64(x) => *x,
            ScalarValue::Int64(x) => *x as f64,
            ScalarValue::Int32(x) => *x as f64,
            _ => 0.0,
        }),
        // A column of nothing but NULLs has no values to store.
        (Buffer::Bytes(_), _) => {}
    }
}

fn set_bit(bitmap: &mut Vec<u8>, index: usize, value: bool) {
    let byte = index / 8;
    while bitmap.len() <= byte {
        bitmap.push(0);
    }
    if value {
        bitmap[byte] |= 1 << (index % 8);
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TableInfo {
    name: String,
    rows: usize,
    columns: Vec<FieldInfo>,
    row_groups: usize,
    bytes: usize,
    pending: bool,
    indexes: Vec<String>,
}

/// A plan as a tree rather than as text, so the page can highlight one node.
///
/// `rel` is the identifier the optimizer stamps on every relational node. A
/// trace step names the node it fired at, which is what lets the slider show
/// *where* a rewrite happened instead of leaving you to diff two blocks of
/// text.
#[derive(Serialize)]
struct PlanNode {
    label: String,
    rel: u32,
    children: Vec<PlanNode>,
}

fn plan_node(plan: &engine::plan::LogicalPlan) -> PlanNode {
    PlanNode {
        label: plan.describe(false),
        rel: plan.rel().0,
        children: plan.children().into_iter().map(plan_node).collect(),
    }
}

#[derive(Serialize)]
struct TraceStepInfo {
    rule: String,
    /// The node the rule fired at.
    target: u32,
    before: PlanNode,
    after: PlanNode,
}

#[derive(Serialize)]
struct ZoneInfo {
    name: String,
    #[serde(rename = "type")]
    data_type: String,
    min: Option<String>,
    max: Option<String>,
    nulls: usize,
    distinct: Option<usize>,
    bloom: bool,
    /// The encoding this column is held in, and how much smaller it made it.
    encoding: Option<String>,
    ratio: Option<f64>,
}

#[derive(Serialize)]
struct RowGroupInfo {
    rows: usize,
    bytes: usize,
    /// `None` for a table that was never lazy; otherwise how many of its
    /// columns have actually been decoded.
    resident: Option<usize>,
    columns: Vec<ZoneInfo>,
}

#[derive(Serialize)]
struct StorageInfo {
    table: String,
    rows: usize,
    bytes: usize,
    pending: bool,
    row_groups: Vec<RowGroupInfo>,
}

#[derive(Serialize)]
struct TreeNodeInfo {
    id: usize,
    level: usize,
    keys: usize,
    first: Option<String>,
    last: Option<String>,
    leaf: bool,
    children: Vec<usize>,
    level_total: usize,
    /// True when this node is on the traversal path a probe took.
    on_path: bool,
}

#[derive(Serialize)]
struct IndexInfo {
    table: String,
    column: String,
    height: usize,
    keys: usize,
    entries: usize,
    nodes: usize,
    leaves: usize,
    bytes: usize,
    level_widths: Vec<usize>,
    tree: Vec<TreeNodeInfo>,
    /// The nodes a probe visited, root first, or empty if none was asked for.
    path: Vec<usize>,
    /// How many rows the probed range matched.
    matched: Option<usize>,
}

#[derive(Serialize)]
struct ExplainInfo {
    tokens: Vec<String>,
    ast: String,
    bound: String,
    optimized: String,
    typed: String,
    steps: Vec<TraceStepInfo>,
    truncated: bool,
}

#[wasm_bindgen]
pub struct QueryEngine {
    inner: Engine,
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// The engine's own surface: plain Rust, plain `String` errors.
///
/// Split from the `#[wasm_bindgen]` block below because `JsValue` cannot be
/// constructed off a wasm target -- it aborts. Keeping the logic here means the
/// conversions are testable under `cargo test` on the host, and the JS surface
/// is a layer of one-line adapters with nothing to get wrong.
impl QueryEngine {
    pub(crate) fn load_inner(&mut self, name: &str, bytes: Vec<u8>) -> Result<String, String> {
        let table = self
            .inner
            .load(name, bytes, &CsvOptions::default())
            .map_err(|d| d.headline())?;
        Ok(self.describe(&table))
    }

    pub(crate) fn query_inner(&mut self, sql: &str) -> Result<QueryOutcome, String> {
        let result = self.inner.execute(sql).map_err(|d| d.render(sql))?;
        Ok(build_outcome(&result))
    }

    pub(crate) fn explain_inner(&self, sql: &str) -> Result<String, String> {
        let tokens = self
            .inner
            .tokenize(sql)
            .map_err(|d| d.render(sql))?
            .iter()
            .map(|t| format!("{:?} {:?}", t.kind, &sql[t.span.start..t.span.end]))
            .collect();
        let ast = self.inner.parse(sql).map_err(|d| d.render(sql))?;
        let bound = self.inner.bound_plan(sql).map_err(|d| d.render(sql))?;
        let optimized = self.inner.plan(sql).map_err(|d| d.render(sql))?;
        let trace = self.inner.optimizer_trace(sql).map_err(|d| d.render(sql))?;

        let info = ExplainInfo {
            tokens,
            ast: format!("{ast:#?}"),
            bound: engine::plan::explain(&bound, false),
            optimized: engine::plan::explain(&optimized, false),
            typed: engine::plan::explain(&optimized, true),
            steps: trace
                .steps
                .iter()
                .map(|s| TraceStepInfo {
                    rule: s.rule.to_string(),
                    target: s.target.0,
                    before: plan_node(&s.before),
                    after: plan_node(&s.after),
                })
                .collect(),
            truncated: trace.truncated,
        };
        serde_json::to_string(&info).map_err(|e| e.to_string())
    }

    pub(crate) fn create_index_inner(
        &mut self,
        table: &str,
        column: &str,
    ) -> Result<String, String> {
        let idx = self
            .inner
            .create_index(table, column)
            .map_err(|d| d.headline())?;
        Ok(format!(
            "{{\"table\":{},\"column\":{},\"keys\":{},\"rows\":{},\"height\":{},\"bytes\":{}}}",
            json_string(&idx.table),
            json_string(&idx.column_name),
            idx.tree.num_keys(),
            idx.tree.num_entries(),
            idx.tree.height(),
            idx.tree.byte_size()
        ))
    }

    /// Row groups, their zone maps, their bloom filters, and -- for a lazily
    /// loaded table -- how much of each has actually been decoded.
    pub(crate) fn storage_inner(&self, table: &str) -> Result<String, String> {
        let t = self
            .inner
            .catalog()
            .get(table, false)
            .ok_or_else(|| format!("no such table `{table}`"))?;

        let info = StorageInfo {
            table: t.name.clone(),
            rows: t.num_rows(),
            bytes: t.byte_size(),
            pending: t.is_pending(),
            row_groups: t
                .row_groups
                .iter()
                .map(|rg| RowGroupInfo {
                    rows: rg.num_rows,
                    bytes: rg.byte_size(),
                    resident: rg.is_pending().then(|| rg.resident_columns()),
                    columns: t
                        .schema
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| ZoneInfo {
                            name: f.name.clone(),
                            data_type: f.data_type.to_string(),
                            min: rg.stats[i].min.as_ref().map(|v| v.to_string()),
                            max: rg.stats[i].max.as_ref().map(|v| v.to_string()),
                            nulls: rg.stats[i].null_count,
                            distinct: rg.stats[i].distinct_count_estimate,
                            bloom: rg.blooms[i].is_some(),
                            encoding: rg.encoding_of(i).map(|(n, _)| n.to_string()),
                            ratio: rg.encoding_of(i).map(|(_, r)| r),
                        })
                        .collect(),
                })
                .collect(),
        };
        serde_json::to_string(&info).map_err(|e| e.to_string())
    }

    /// The shape of a B+ tree, and optionally the path a probe took through it.
    ///
    /// `probe` is parsed against the indexed column's type; when it is given,
    /// the returned `path` is the node at each level the descent visited and
    /// `matched` is how many rows the key actually names. That pairing --
    /// structure plus the route through it -- is the thing worth drawing.
    pub(crate) fn index_tree_inner(
        &self,
        table: &str,
        column: &str,
        probe: Option<&str>,
        max_per_level: usize,
    ) -> Result<String, String> {
        let t = self
            .inner
            .catalog()
            .get(table, false)
            .ok_or_else(|| format!("no such table `{table}`"))?;
        let index = t
            .schema
            .fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(column))
            .and_then(|i| self.inner.catalog().index_on(&t.name, i))
            .ok_or_else(|| format!("no index on `{table}.{column}`"))?;

        let field = &t.schema.field(index.column);
        let key = probe
            .filter(|p| !p.trim().is_empty())
            .map(|p| parse_key(p, &field.data_type))
            .transpose()?;

        let path = key.as_ref().map_or_else(Vec::new, |k| index.tree.path_to(k));
        let matched = key.as_ref().map(|k| index.tree.lookup(k).len());

        let info = IndexInfo {
            table: index.table.clone(),
            column: index.column_name.clone(),
            height: index.tree.height(),
            keys: index.tree.num_keys(),
            entries: index.tree.num_entries(),
            nodes: index.tree.num_nodes(),
            leaves: index.tree.num_leaves(),
            bytes: index.tree.byte_size(),
            level_widths: index.tree.level_widths(),
            tree: index
                .tree
                .describe(max_per_level.clamp(1, 64), &path)
                .into_iter()
                .map(|n| TreeNodeInfo {
                    on_path: path.contains(&n.id),
                    id: n.id,
                    level: n.level,
                    keys: n.keys,
                    first: n.first.map(|v| v.to_string()),
                    last: n.last.map(|v| v.to_string()),
                    leaf: n.leaf,
                    children: n.children,
                    level_total: n.level_total,
                })
                .collect(),
            path,
            matched,
        };
        serde_json::to_string(&info).map_err(|e| e.to_string())
    }

    pub(crate) fn tables_inner(&self) -> String {
        let infos: Vec<String> = self
            .inner
            .catalog()
            .tables()
            .iter()
            .map(|t| self.describe(t))
            .collect();
        format!("[{}]", infos.join(","))
    }

    fn describe(&self, table: &Arc<engine::storage::Table>) -> String {
        let info = TableInfo {
            name: table.name.clone(),
            rows: table.num_rows(),
            columns: table
                .schema
                .fields
                .iter()
                .map(|f| FieldInfo {
                    name: f.name.clone(),
                    data_type: f.data_type.to_string(),
                    nullable: f.nullable,
                    kind: kind_of(&f.data_type) as u8,
                })
                .collect(),
            row_groups: table.num_row_groups(),
            bytes: table.byte_size(),
            pending: table.is_pending(),
            indexes: self
                .inner
                .catalog()
                .indexes_on(&table.name)
                .iter()
                .map(|i| i.column_name.clone())
                .collect(),
        };
        serde_json::to_string(&info).unwrap_or_else(|_| "{}".into())
    }
}

#[wasm_bindgen]
impl QueryEngine {
    #[wasm_bindgen(constructor)]
    pub fn new() -> QueryEngine {
        QueryEngine {
            inner: Engine::new(),
        }
    }

    /// Register a buffer as a table, choosing CSV or Parquet by its first
    /// bytes. The page fetches these; nothing here does any I/O.
    pub fn load(&mut self, name: &str, bytes: Vec<u8>) -> Result<String, JsValue> {
        self.load_inner(name, bytes).map_err(js)
    }

    pub fn tables(&self) -> String {
        self.tables_inner()
    }

    /// Row groups, zone maps, bloom filters and decode residency.
    pub fn storage(&self, table: &str) -> Result<String, JsValue> {
        self.storage_inner(table).map_err(js)
    }

    /// A B+ tree's shape, and the path a probe takes through it.
    #[wasm_bindgen(js_name = indexTree)]
    pub fn index_tree(
        &self,
        table: &str,
        column: &str,
        probe: Option<String>,
        max_per_level: usize,
    ) -> Result<String, JsValue> {
        self.index_tree_inner(table, column, probe.as_deref(), max_per_level)
            .map_err(js)
    }

    /// Run a query and keep its columns in wasm memory.
    pub fn query(&mut self, sql: &str) -> Result<QueryOutcome, JsValue> {
        self.query_inner(sql).map_err(js)
    }

    /// Every stage of the pipeline, for the plan panels: tokens, parse tree,
    /// the plan as bound, the optimized plan, and each rewrite in between.
    pub fn explain(&self, sql: &str) -> Result<String, JsValue> {
        self.explain_inner(sql).map_err(js)
    }

    #[wasm_bindgen(js_name = createIndex)]
    pub fn create_index(&mut self, table: &str, column: &str) -> Result<String, JsValue> {
        self.create_index_inner(table, column).map_err(js)
    }

    /// Run one file of the sqllogictest corpus, so the browser build can be
    /// checked against the same expectations the native build is.
    #[wasm_bindgen(js_name = runSlt)]
    pub fn run_slt(&mut self, text: &str, csv: Vec<u8>) -> String {
        let mut resolve = |_: &str| Ok(csv.clone());
        match engine::sqllogictest::run(text, &mut resolve) {
            Ok(report) => format!(
                "{{\"passed\":{},\"skipped\":{},\"failed\":{}}}",
                report.passed,
                report.skipped,
                report.failures.len()
            ),
            Err(e) => format!("{{\"error\":{}}}", json_string(&e.to_string())),
        }
    }
}

/// Diagnostics reach the page already rendered -- caret underline and all --
/// so the browser shows the same error the terminal would.
fn js(message: String) -> JsValue {
    JsValue::from_str(&message)
}

/// Parse a probe typed into the UI against the indexed column's type.
///
/// A text box gives strings; the tree is ordered by the column's own type, so
/// `"42"` must become an integer or the comparison is lexicographic and the
/// path drawn would be a lie.
fn parse_key(text: &str, data_type: &DataType) -> Result<ScalarValue, String> {
    let text = text.trim();
    let bad = || format!("`{text}` is not a valid {data_type} value");
    Ok(match data_type {
        DataType::Int32 => ScalarValue::Int32(text.parse().map_err(|_| bad())?),
        DataType::Int64 => ScalarValue::Int64(text.parse().map_err(|_| bad())?),
        DataType::Float64 => ScalarValue::Float64(text.parse().map_err(|_| bad())?),
        DataType::Boolean => ScalarValue::Boolean(matches!(
            text.to_ascii_lowercase().as_str(),
            "true" | "t" | "1" | "yes"
        )),
        DataType::Utf8 => ScalarValue::Utf8(text.to_string()),
        // Dates and timestamps arrive as text and go through the same cast the
        // binder would apply to a string literal, so the UI agrees with SQL.
        other => engine::types::cast_scalar(&ScalarValue::Utf8(text.to_string()), other)
            .map_err(|_| bad())?,
    })
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests;
