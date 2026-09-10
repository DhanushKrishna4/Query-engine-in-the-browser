// The page. Everything below the boundary is Rust; this file only fetches
// data, hands buffers in, and reads buffers out.
import init, { QueryEngine, ColumnKind } from "../pkg/qe.js";
// The bundler hashes and moves the module, so its size is asked of the URL it
// ends up at rather than of a path written down here.
import wasmUrl from "../pkg/qe_bg.wasm?url";
import { SqlEditor, type SpanError } from "./editor";
import type {
  CheckInfo,
  ExplainInfo,
  IndexInfo,
  OutcomeMeta,
  PhysicalNode,
  PlanNode,
  StatsInfo,
  StorageInfo,
  GroupVerdict,
  TableInfo,
  TokenInfo,
  TreeNodeInfo,
  TraceStep,
} from "./types";

type Outcome = ReturnType<QueryEngine["query"]>;

/** The wasm module's exports, for reading typed arrays out of its memory. */
let wasm: { memory: WebAssembly.Memory };
let engine: QueryEngine;
let editor: SqlEditor;
let storageTable = "people";
/** The `.wasm`'s size, and whether the server compressed it on the way. */
let wasmSize = { bytes: 0, compressed: false };
let traceSteps: TraceStep[] = [];
let indexChoice: { table: string; column: string } | null = null;

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

// Small enough to ship beside the page, and loaded before anything is drawn so
// the first query runs the moment the page does. Anything larger is a
// `COLLECTIONS` entry below -- the engine never does I/O itself, so a dataset
// is just a byte buffer that arrived from somewhere.
const DATASETS = [
  { name: "people", url: "data/people.csv" },
  // `data/orders.csv` under a name TPC-H does not also use. Loading the TPC-H
  // collection brings its own `orders`, and a demo table quietly replaced by a
  // benchmark table would break the join example on the same page.
  { name: "purchases", url: "data/orders.csv" },
  // Parquet, eight row groups: the table the storage inspector is worth
  // opening on, because its zone maps come from the file's own footer and its
  // columns are decoded only when a scan asks.
  { name: "metrics", url: "data/metrics.parquet" },
];

/** A group of tables fetched together, on request, from a CDN. */
interface Collection {
  id: string;
  label: string;
  /** What the button says about the cost of pressing it. */
  size: string;
  blurb: string;
  tables: { name: string; url: string }[];
  /** Added to the example list once the tables behind them exist. */
  examples: Example[];
}

/**
 * The real datasets.
 *
 * Not committed to this repository and not built into the page: fetched from a
 * CDN when asked for, then held in IndexedDB so a second visit costs nothing.
 * Both are opt-in because both are large enough that downloading them on every
 * page load would be rude -- and because the point of the first three tables
 * above is that the page is usable before either arrives.
 *
 * Parquet is the format for both, which is the whole reason the reader exists:
 * the taxi file is 127 MB and 114 row groups, and a query that names two of
 * its eighteen columns decodes two of them, in the row groups its zone maps
 * could not rule out.
 */
const COLLECTIONS: Collection[] = [
  {
    id: "tpch",
    label: "TPC-H",
    size: "8 tables · ~7 MB",
    blurb:
      "the standard join benchmark schema at scale factor 0.01. Eight tables, foreign keys throughout, and the queries the join reordering and decorrelation rules were written for.",
    tables: [
      "customer",
      "lineitem",
      "nation",
      "orders",
      "part",
      "partsupp",
      "region",
      "supplier",
    ].map((name) => ({
      name,
      url: `https://shell.duckdb.org/data/tpch/0_01/parquet/${name}.parquet`,
    })),
    examples: [
      {
        label: "TPC-H Q3 · shipping priority",
        sql:
          "SELECT l.l_orderkey,\n" +
          "       SUM(l.l_extendedprice * (1 - l.l_discount)) AS revenue,\n" +
          "       o.o_orderdate, o.o_shippriority\n" +
          "FROM customer c\n" +
          "JOIN orders o ON c.c_custkey = o.o_custkey\n" +
          "JOIN lineitem l ON l.l_orderkey = o.o_orderkey\n" +
          "WHERE c.c_mktsegment = 'BUILDING'\n" +
          "  AND o.o_orderdate < DATE '1995-03-15'\n" +
          "  AND l.l_shipdate > DATE '1995-03-15'\n" +
          "GROUP BY l.l_orderkey, o.o_orderdate, o.o_shippriority\n" +
          "ORDER BY revenue DESC, o.o_orderdate\n" +
          "LIMIT 10",
        watch:
          "physical plan → the three-way join order. DPsize costs every shape and builds the hash table on customer, the smallest side after its filter, not on lineitem.",
      },
      {
        label: "TPC-H Q6 · forecasting revenue change",
        sql:
          "SELECT SUM(l_extendedprice * l_discount) AS revenue\n" +
          "FROM lineitem\n" +
          "WHERE l_shipdate >= DATE '1994-01-01'\n" +
          "  AND l_shipdate < DATE '1995-01-01'\n" +
          "  AND l_discount BETWEEN 0.05 AND 0.07\n" +
          "  AND l_quantity < 24",
        watch:
          "execution → the scan's row-group line. A date range over sorted-ish data is what zone maps are for: whole row groups are skipped on their footer bounds without a byte being decompressed.",
      },
      {
        label: "TPC-H Q4 · order priority, as a semi join",
        sql:
          "SELECT o.o_orderpriority, COUNT(*) AS order_count\n" +
          "FROM orders o\n" +
          "WHERE o.o_orderdate >= DATE '1993-07-01'\n" +
          "  AND o.o_orderdate < DATE '1993-10-01'\n" +
          "  AND EXISTS (\n" +
          "    SELECT 1 FROM lineitem l\n" +
          "    WHERE l.l_orderkey = o.o_orderkey AND l.l_commitdate < l.l_receiptdate\n" +
          "  )\n" +
          "GROUP BY o.o_orderpriority\n" +
          "ORDER BY o.o_orderpriority",
        watch:
          "optimizer trace → decorrelate. The EXISTS becomes a semi join, so lineitem is scanned once instead of once per qualifying order.",
      },
    ],
  },
  {
    id: "taxi",
    label: "NYC taxi",
    size: "7.4M rows · 127 MB",
    blurb:
      "every yellow cab trip in New York in April 2019, as the city published it. Large enough that zone-map pruning and lazy column decoding stop being a demonstration and start being the reason a query returns at all.",
    tables: [{ name: "trips", url: "https://blobs.duckdb.org/data/taxi_2019_04.parquet" }],
    examples: [
      {
        label: "taxi · fares by passenger count",
        sql:
          "SELECT passenger_count,\n" +
          "       COUNT(*) AS trips,\n" +
          "       AVG(total_amount) AS avg_fare\n" +
          "FROM trips\n" +
          "WHERE trip_distance > 5\n" +
          "GROUP BY passenger_count\n" +
          "ORDER BY trips DESC",
        watch:
          "storage → 114 row groups, and the scan decoded three of eighteen columns. The other fifteen were never touched: that is the whole argument for columnar storage, on 7.4 million rows.",
      },
      {
        label: "taxi · the busiest pickup zones",
        sql:
          "SELECT pickup_location_id,\n" +
          "       COUNT(*) AS trips,\n" +
          "       AVG(trip_distance) AS avg_miles,\n" +
          "       AVG(tip_amount) AS avg_tip\n" +
          "FROM trips\n" +
          "GROUP BY pickup_location_id\n" +
          "ORDER BY trips DESC\n" +
          "LIMIT 15",
        watch:
          "execution → estimated against actual rows per operator. A group-by over 260 zones on 7.4M rows is where the HyperLogLog distinct estimate earns its keep, and where you can see how close it got.",
      },
      {
        label: "taxi · the long tail",
        sql:
          "SELECT vendor_id, pickup_at, trip_distance, total_amount\n" +
          "FROM trips\n" +
          "WHERE trip_distance > 60\n" +
          "ORDER BY trip_distance DESC\n" +
          "LIMIT 20",
        watch:
          "physical plan → TopN, and the scan's pruning count. Twenty rows are wanted out of 7.4 million, and most row groups cannot hold a 60-mile trip at all.",
      },
    ],
  },
];

/** Collections already in the catalog, so a second press is a no-op. */
const loaded = new Set<string>();

const DB_NAME = "qe-cache";
const STORE = "datasets";

/** Open the IndexedDB cache, or resolve to null if the browser refuses. */
function openDb(): Promise<IDBDatabase | null> {
  return new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB_NAME, 1);
    } catch {
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    // A private window, a browser with site data blocked, a quota refusal:
    // all of them mean "no cache", none of them mean "no engine".
    request.onerror = () => resolve(null);
  });
}

function idb(
  db: IDBDatabase,
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest
): Promise<any> {
  return new Promise((resolve) => {
    try {
      const tx = db.transaction(STORE, mode);
      const request = fn(tx.objectStore(STORE));
      request.onsuccess = () => resolve(request.result ?? null);
      request.onerror = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
}

/**
 * Fetch a dataset, using IndexedDB as a cache.
 *
 * The cache is what makes a multi-megabyte dataset bearable on a second visit:
 * the bytes are stored exactly as they arrived, so a Parquet file stays
 * compressed on disk and is decoded lazily by the engine either way.
 */
async function fetchDataset(
  url: string,
  db: IDBDatabase | null,
  onProgress?: (received: number, total: number) => void
): Promise<{ bytes: Uint8Array; cached: boolean }> {
  if (db) {
    const cached = await idb(db, "readonly", (store) => store.get(url));
    if (cached) return { bytes: new Uint8Array(cached), cached: true };
  }
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status} ${response.statusText}`);
  const bytes = new Uint8Array(await readAll(response, onProgress));
  if (db) await idb(db, "readwrite", (store) => store.put(bytes.buffer, url));
  return { bytes, cached: false };
}

/**
 * Read a response body, reporting bytes as they arrive.
 *
 * `arrayBuffer()` would do were it not for the 127 MB file: a minute of blank
 * page with no sign of progress is indistinguishable from a hang. Falls back to
 * the simple path where the body cannot be streamed.
 */
async function readAll(
  response: Response,
  onProgress?: (received: number, total: number) => void
): Promise<ArrayBuffer> {
  const total = Number(response.headers.get("content-length") ?? 0);
  if (!response.body || !onProgress) return response.arrayBuffer();

  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let received = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    received += value.length;
    onProgress(received, total);
  }
  const out = new Uint8Array(received);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.length;
  }
  return out.buffer;
}

// ---------------------------------------------------------------------------
// Reading results out of wasm memory
// ---------------------------------------------------------------------------

/**
 * A column as typed-array views onto the module's memory.
 *
 * `values` is deliberately loose: which typed array it is depends on `kind`,
 * and the switch in `cell` is the only place that knows. Naming the union here
 * would mean narrowing it again at every use for no gain.
 */
interface ColumnView {
  kind: number;
  rows: number;
  validity: Uint8Array | null;
  values: any;
  offsets: Uint32Array | null;
}

/**
 * Build typed-array views onto a result column, without copying it.
 *
 * `memory.buffer` is re-read on every call and no view outlives this function,
 * because allocating inside wasm can grow the memory and growing detaches every
 * ArrayBuffer built on the old one.
 */
function columnViews(outcome: Outcome, index: number): ColumnView {
  const buffer = wasm.memory.buffer;
  const kind = outcome.kind(index);
  const rows = outcome.len(index);
  const ptr = outcome.valuesPtr(index);
  const validityPtr = outcome.validityPtr(index);
  // A null pointer means no row in the column is NULL, so the per-row check
  // can be skipped entirely.
  const validity = validityPtr === 0 ? null : new Uint8Array(buffer, validityPtr, Math.ceil(rows / 8));

  let values = null;
  let offsets = null;
  switch (kind) {
    case ColumnKind.Int32:
    case ColumnKind.Date32:
      values = new Int32Array(buffer, ptr, rows);
      break;
    case ColumnKind.Int64:
    case ColumnKind.Timestamp:
      values = new BigInt64Array(buffer, ptr, rows);
      break;
    case ColumnKind.Float64:
      values = new Float64Array(buffer, ptr, rows);
      break;
    case ColumnKind.Boolean:
      values = new Uint8Array(buffer, ptr, Math.ceil(rows / 8));
      break;
    case ColumnKind.Utf8:
      values = new Uint8Array(buffer, ptr, outcome.valuesBytes(index));
      offsets = new Uint32Array(buffer, outcome.offsetsPtr(index), rows + 1);
      break;
    default:
      break;
  }
  return { kind, rows, values, offsets, validity };
}

const decoder = new TextDecoder();
const MS_PER_DAY = 86400000;

function cell(view: ColumnView, row: number): string | null {
  if (view.validity && (view.validity[row >> 3] & (1 << (row & 7))) === 0) return null;
  switch (view.kind) {
    case ColumnKind.Boolean:
      return String((view.values[row >> 3] & (1 << (row & 7))) !== 0);
    case ColumnKind.Int32:
    case ColumnKind.Float64:
      return String(view.values[row]);
    case ColumnKind.Int64:
      // A BigInt64Array element, so `String` rather than a template literal --
      // the latter is the same thing but reads as if it might be a number.
      return String(view.values[row]);
    case ColumnKind.Date32:
      return new Date(view.values[row] * MS_PER_DAY).toISOString().slice(0, 10);
    case ColumnKind.Timestamp:
      // Microseconds since the epoch. Milliseconds is all a JS Date holds, so
      // the sub-millisecond part is printed separately rather than dropped.
      return new Date(Number(view.values[row] / 1000n)).toISOString().replace("T", " ").replace("Z", "");
    case ColumnKind.Utf8:
      return view.offsets
        ? decoder.decode(view.values.subarray(view.offsets[row], view.offsets[row + 1]))
        : null;
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/**
 * An element that must exist.
 *
 * Every id here is written in `index.html`, so a missing one is a typo in this
 * file rather than a condition to handle -- and throwing names it, where
 * returning null would surface as `Cannot read properties of null` twenty
 * lines away.
 */
const $ = <T extends HTMLElement = HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`no element with id "${id}"`);
  return el as T;
};

/**
 * The message out of a caught value.
 *
 * What crosses the wasm boundary as an error is a JS `Error` in practice, but
 * `catch` types it `unknown` and the page must render *something* either way.
 */
const message = (e: unknown): string =>
  e instanceof Error ? e.message : String(e);

/** The header row for a result, built once whether or not it streams. */
function resultTable(meta: OutcomeMeta): HTMLTableElement {
  const table = document.createElement("table");
  const head = table.createTHead().insertRow();
  for (const column of meta.columns) {
    const th = document.createElement("th");
    th.textContent = column.name;
    th.title = column.type + (column.nullable ? " (nullable)" : "");
    head.appendChild(th);
  }
  table.createTBody();
  return table;
}

/**
 * Append one chunk's rows, up to `budget` of them.
 *
 * Returns how many it wrote. The views are built here and used here: a typed
 * array over wasm memory is valid only until wasm allocates again, and the
 * next chunk allocates.
 */
function appendRows(
  table: HTMLTableElement,
  outcome: Outcome,
  meta: OutcomeMeta,
  budget: number
): number {
  const views = meta.columns.map((_, i) => columnViews(outcome, i));
  const available = Math.min(views[0]?.rows ?? 0, budget);
  const tbody = table.tBodies[0];
  for (let row = 0; row < available; row++) {
    const tr = tbody.insertRow();
    for (let c = 0; c < views.length; c++) {
      const td = tr.insertCell();
      const value = cell(views[c], row);
      if (value === null) {
        td.className = "null";
        td.textContent = "NULL";
      } else {
        td.className = views[c].kind === ColumnKind.Utf8 ? "" : "num";
        td.textContent = value;
      }
    }
  }
  return available;
}

function renderResults(outcome: Outcome, meta: OutcomeMeta) {
  const body = $("tab-results");
  body.textContent = "";
  if (meta.columns.length === 0) {
    body.innerHTML = '<p class="empty">no columns</p>';
    return;
  }

  const table = resultTable(meta);
  const rows = appendRows(table, outcome, meta, Infinity);
  if (rows === 0) {
    body.innerHTML = '<p class="empty">no rows</p>';
    return;
  }
  body.appendChild(table);
  if (meta.truncated) {
    const note = document.createElement("p");
    note.className = "empty";
    note.textContent = `showing the first ${rows.toLocaleString()} of ${meta.num_rows.toLocaleString()} rows — the query computed all of them`;
    body.appendChild(note);
  }
}

/** The operator tree, with each node's exclusive share of the time. */
/**
 * What the last query's scans did to each table's row groups.
 *
 * Recorded here rather than passed down because the storage inspector is drawn
 * when its tab is opened, which is usually several queries later, and the
 * point of the panel is to show what the query you just ran actually skipped.
 */
const lastScans = new Map<string, GroupVerdict[]>();

function collectScans(node: StatsInfo) {
  if (node.scanned_table && node.row_group_verdicts.length > 0) {
    lastScans.set(node.scanned_table, node.row_group_verdicts);
  }
  node.children.forEach(collectScans);
}

function renderPipeline(stats: StatsInfo) {
  lastScans.clear();
  collectScans(stats);
  const body = $("tab-pipeline");
  body.textContent = "";
  let total = 0;
  (function sum(node) {
    total += node.elapsed_ms;
    node.children.forEach(sum);
  })(stats);

  const header = document.createElement("div");
  header.className = "op";
  header.innerHTML =
    '<div class="op-name" style="color:var(--dim)">operator</div>' +
    '<div class="op-num">rows out</div><div class="op-num">estimated</div><div class="op-num">time</div>';
  body.appendChild(header);

  (function walk(node, depth) {
    const row = document.createElement("div");
    row.className = "op";

    const name = document.createElement("div");
    name.className = "op-name";
    const share = total > 0 ? node.elapsed_ms / total : 0;
    name.innerHTML =
      "&nbsp;".repeat(depth * 2) +
      `<b>${escapeHtml(node.name)}</b> <span class="detail">${escapeHtml(node.detail)}</span>` +
      (node.row_groups_total
        ? ` <span class="detail">[${node.row_groups_scanned}/${node.row_groups_total} row groups read` +
          (node.row_groups_bloom_pruned ? `, ${node.row_groups_bloom_pruned} by bloom` : "") +
          "]</span>"
        : "") +
      `<div class="bar" style="width:${(share * 100).toFixed(1)}%"></div>`;

    const rowsOut = document.createElement("div");
    rowsOut.className = "op-num";
    rowsOut.textContent = node.rows_out.toLocaleString();

    const est = document.createElement("div");
    est.className = "op-num";
    if (node.estimated_rows !== null && node.estimated_rows !== undefined) {
      const q = node.q_error ?? 1;
      est.innerHTML =
        `${Math.round(node.estimated_rows).toLocaleString()} ` +
        `<span class="${q >= 10 ? "qerr-bad" : ""}">(${q.toFixed(1)}x)</span>`;
      est.title = "estimated rows, and the q-error against what actually came out";
    } else {
      est.textContent = "—";
    }

    // One dot per batch handed to the operator above, capped so a long query
    // does not paint thousands. This is the pipeline actually moving: the
    // engine is batched, and the count is what it really produced.
    if (node.batches > 0) {
      const flow = document.createElement("div");
      flow.className = "flow";
      const shown = Math.min(node.batches, 24);
      for (let i = 0; i < shown; i++) {
        const dot = document.createElement("span");
        dot.className = "batch";
        dot.style.animationDelay = `${(i * 60) % 1600}ms`;
        flow.appendChild(dot);
      }
      if (node.batches > shown) {
        const more = document.createElement("span");
        more.className = "more";
        more.textContent = `+${(node.batches - shown).toLocaleString()}`;
        flow.appendChild(more);
      }
      flow.title = `${node.batches.toLocaleString()} batch${node.batches === 1 ? "" : "es"} of up to 2048 rows`;
      name.appendChild(flow);
    }

    const time = document.createElement("div");
    time.className = "op-num";
    time.textContent = node.elapsed_ms < 0.01 ? "<0.01 ms" : `${node.elapsed_ms.toFixed(2)} ms`;

    row.append(name, rowsOut, est, time);
    body.appendChild(row);
    node.children.forEach((child) => walk(child, depth + 1));
  })(stats, 0);
}

/**
 * The lexer's output, in order.
 *
 * The first stage of the pipeline and the only one that is a flat list. Each
 * chip carries the span it was read from, so pointing at one underlines the
 * characters it covers in the editor -- which is the whole point of a tokens
 * view: seeing that `<=` is one token and `< =` is two, that a quoted
 * identifier is not a keyword, that whitespace left no trace.
 */
function renderTokens(explain: ExplainInfo) {
  const body = $("tab-tokens");
  // The EOF token has an empty span and nothing to show for it.
  const tokens = explain.tokens.filter((t) => t.kind !== "Eof");
  body.innerHTML =
    `<p class="empty" style="margin:0 0 .8rem">${tokens.length} token${tokens.length === 1 ? "" : "s"}. ` +
    `Hover one to see where it came from.</p>` +
    `<div class="tokens">` +
    tokens.map((t, i) => `<span class="token" data-i="${i}">${escapeHtml(t.text)}` +
      `<span class="token-kind">${escapeHtml(tokenKind(t))}</span></span>`).join("") +
    `</div>`;

  for (const chip of body.querySelectorAll<HTMLElement>(".token")) {
    const token = tokens[Number(chip.dataset.i)];
    chip.addEventListener("mouseenter", () => editor.highlight(token.start, token.end));
    chip.addEventListener("mouseleave", () => editor.highlight(null, null));
  }
}

/** `Keyword(Select)` reads better as `keyword`: the chip already says which. */
function tokenKind(token: TokenInfo): string {
  return token.kind.startsWith("Keyword(") ? "keyword" : token.kind.toLowerCase();
}

/**
 * The parse tree, collapsible.
 *
 * `ast::pretty` writes two spaces per level and one node per line, so the
 * indentation *is* the structure and reading it back is a matter of counting
 * spaces. Building the tree here rather than shipping a second serialization
 * from Rust keeps one description of the AST rather than two that can drift --
 * and that one is already pinned by the parser's snapshot tests.
 */
function renderAst(explain: ExplainInfo) {
  const body = $("tab-ast");
  const lines = explain.ast.split("\n").filter((l) => l.trim() !== "");

  interface AstNode {
    label: string;
    children: AstNode[];
  }
  const root: AstNode = { label: "", children: [] };
  // The node most recently seen at each depth, so a line's parent is whatever
  // sits one level shallower.
  const open: AstNode[] = [root];
  for (const line of lines) {
    const depth = (line.length - line.trimStart().length) / 2 + 1;
    const node: AstNode = { label: line.trim(), children: [] };
    (open[depth - 1] ?? root).children.push(node);
    open[depth] = node;
    open.length = depth + 1;
  }

  const draw = (node: AstNode, depth: number): string => {
    if (node.children.length === 0) {
      return `<li class="ast-leaf">${escapeHtml(node.label)}</li>`;
    }
    // Open to three levels: enough to see the shape of a SELECT, not so much
    // that a five-way join fills the screen before anyone has looked at it.
    return (
      `<li><details${depth < 3 ? " open" : ""}><summary>${escapeHtml(node.label)}</summary>` +
      `<ul>${node.children.map((c) => draw(c, depth + 1)).join("")}</ul></details></li>`
    );
  };
  body.innerHTML = `<ul class="ast">${root.children.map((c) => draw(c, 0)).join("")}</ul>`;
}

function renderPlan(explain: ExplainInfo) {
  const body = $("tab-plan");
  body.innerHTML =
    `<h4 style="color:var(--dim);font-size:.7rem;margin:0 0 .4rem">OPTIMIZED PLAN</h4><pre>${escapeHtml(explain.optimized)}</pre>` +
    `<h4 style="color:var(--dim);font-size:.7rem;margin:1.2rem 0 .4rem">WITH RESOLVED TYPES</h4><pre>${escapeHtml(explain.typed)}</pre>` +
    `<h4 style="color:var(--dim);font-size:.7rem;margin:1.2rem 0 .4rem">AS BOUND, BEFORE ANY REWRITE</h4><pre>${escapeHtml(explain.bound)}</pre>`;
}

/**
 * The physical plan: which operator, and why that one.
 *
 * Built without running the query, so this is the decision the engine made
 * rather than a report of what happened. Nodes that had no alternative -- a
 * projection, a filter -- state no reason, which is the honest thing for them
 * to say.
 */
function renderPhysical(sql: string) {
  const body = $("tab-physical");
  let root;
  try {
    root = JSON.parse(engine.physicalPlan(sql));
  } catch (e) {
    body.innerHTML = `<div class="error">${escapeHtml(message(e))}</div>`;
    return;
  }
  const lines: string[] = [];
  (function walk(node: PhysicalNode, depth: number) {
    const pad = "  ".repeat(depth);
    const arrow = depth > 0 ? "-> " : "";
    const est =
      node.estimated_rows === null || node.estimated_rows === undefined
        ? ""
        : ` <span class="phys-est">est ${Math.round(node.estimated_rows).toLocaleString()} rows</span>`;
    lines.push(
      `<div class="phys-node">${pad}${arrow}<span class="phys-op">${escapeHtml(node.name)}</span>${est}</div>`
    );
    if (node.detail) {
      lines.push(`<div class="phys-node phys-detail">${pad}     ${escapeHtml(node.detail)}</div>`);
    }
    if (node.reason) {
      lines.push(`<div class="phys-node phys-why">${pad}     ${escapeHtml(node.reason)}</div>`);
    }
    node.children.forEach((c: PhysicalNode) => walk(c, depth + 1));
  })(root as PhysicalNode, 0);
  body.innerHTML = `<div class="phys">${lines.join("")}</div>`;
}

/** Collect the rel ids in a subtree, so the whole of it can be highlighted. */
function subtreeRels(
  node: PlanNode,
  target: number,
  found = new Set<number>(),
  inside = false
): Set<number> {
  const here = inside || node.rel === target;
  if (here) found.add(node.rel);
  for (const child of node.children) subtreeRels(child, target, found, here);
  return found;
}

/** Render a plan as an indented tree, marking the subtree a rule fired at. */
function renderPlanTree(root: PlanNode, target: number): string {
  const marked = target === null || target === undefined ? new Set() : subtreeRels(root, target);
  const lines = [];
  (function walk(node, depth) {
    const cls =
      node.rel === target ? "plan-node target" : marked.has(node.rel) ? "plan-node changed" : "plan-node";
    const arrow = depth > 0 ? "-> " : "";
    lines.push(`<div class="${cls}">${"  ".repeat(depth)}${arrow}${escapeHtml(node.label)}</div>`);
    node.children.forEach((child) => walk(child, depth + 1));
  })(root, 0);
  return `<div class="plan-tree">${lines.join("")}</div>`;
}


/**
 * The optimizer trace, one rewrite at a time.
 *
 * Each step names the node the rule fired at, so the subtree it touched is
 * highlighted rather than left to be found by diffing two blocks of text.
 */
function renderTrace(explain: ExplainInfo) {
  const body = $("tab-trace");
  traceSteps = explain.steps;
  if (traceSteps.length === 0) {
    body.innerHTML = '<p class="empty">the optimizer changed nothing — this plan was already in its final form</p>';
    return;
  }
  body.innerHTML =
    `<div class="slider-bar">` +
    `<button id="step-prev">←</button>` +
    `<input type="range" id="step-slider" min="1" max="${traceSteps.length}" value="1">` +
    `<button id="step-next">→</button>` +
    `<span class="slider-label" id="step-label"></span></div>` +
    `<div id="step-view"></div>`;

  const slider = $<HTMLInputElement>("step-slider");
  const show = (n: number) => {
    const step = traceSteps[n - 1];
    $("step-label").innerHTML =
      `step <b>${n}</b> of ${traceSteps.length} &nbsp;·&nbsp; <b>${escapeHtml(step.rule)}</b> at #${step.target}`;
    $("step-view").innerHTML =
      `<div class="side-by-side"><div><h4>before</h4>${renderPlanTree(step.before, step.target)}</div>` +
      `<div><h4>after</h4>${renderPlanTree(step.after, step.target)}</div></div>`;
    slider.value = String(n);
  };
  slider.addEventListener("input", () => show(Number(slider.value)));
  $("step-prev").addEventListener("click", () => show(Math.max(1, Number(slider.value) - 1)));
  $("step-next").addEventListener("click", () => show(Math.min(traceSteps.length, Number(slider.value) + 1)));
  show(1);
}

// ---------------------------------------------------------------------------
// Storage inspector
// ---------------------------------------------------------------------------

/** The border colour for a row group, by what happened to it. */
function verdictClass(verdict: GroupVerdict | undefined): string {
  switch (verdict) {
    case "scanned":
      return " rg-scanned";
    case undefined:
    case "untouched":
      return "";
    default:
      return " rg-pruned";
  }
}

/** A chip naming what skipped a row group, or that it was read. */
function verdictChip(verdict: GroupVerdict | undefined): string {
  if (verdict === undefined || verdict === "untouched") return "";
  if (verdict === "scanned") return ` <span class="chip">scanned</span>`;
  // Which structure ruled it out, because they rule out different shapes of
  // predicate and knowing which one fired is the point of showing this at all.
  return ` <span class="chip pruned">pruned · ${escapeHtml(verdict)}</span>`;
}

/** Bytes at a scale a reader can hold in their head. */
function humanBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB"];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

/**
 * How many row groups to draw.
 *
 * A 114-group Parquet file is 2,000 table rows of zone map, which is both slow
 * to paint and more than anyone reads. The first dozen make the point -- the
 * bounds are per group, they are disjoint where the data is ordered, and most
 * groups hold nothing a given predicate wants.
 */
const ROW_GROUPS_SHOWN = 12;

function renderStorage(table: string) {
  storageTable = table;
  const body = $("tab-storage");
  let info: StorageInfo;
  try {
    info = JSON.parse(engine.storage(table));
  } catch (e) {
    body.innerHTML = `<p class="empty">${escapeHtml(message(e))}</p>`;
    return;
  }

  const head =
    `<div class="tree-controls"><span class="slider-label">` +
    `<b>${escapeHtml(info.table)}</b> · ${info.rows.toLocaleString()} rows · ` +
    `${info.row_groups.length} row group${info.row_groups.length === 1 ? "" : "s"} · ` +
    `${humanBytes(info.bytes)}` +
    (info.pending ? " · lazily decoded from Parquet" : " · fully resident") +
    `</span>` +
    tableSwitcher(table, renderStorage) +
    `</div>`;

  const verdicts = lastScans.get(table) ?? [];
  const skipped = verdicts.filter((v) => v !== "scanned" && v !== "untouched").length;

  const groups = info.row_groups
    .slice(0, ROW_GROUPS_SHOWN)
    .map((rg, i) => {
      const rows = rg.columns
        .map(
          (c) =>
            `<tr><td>${escapeHtml(c.name)}</td><td style="color:var(--dim)">${escapeHtml(c.type.toLowerCase())}</td>` +
            `<td>${c.min === null ? "—" : escapeHtml(c.min)}</td><td>${c.max === null ? "—" : escapeHtml(c.max)}</td>` +
            `<td class="num">${c.nulls === null ? "?" : c.nulls}</td>` +
            `<td class="num">${c.distinct === null ? "&gt;8192" : c.distinct}</td>` +
            `<td>${
              c.encoding === null
                ? '<span class="chip off">plain</span>'
                : `<span class="chip">${escapeHtml(c.encoding)}</span>` +
                  (c.ratio === null ? "" : ` <span class="dim">${c.ratio.toFixed(1)}x</span>`)
            }</td>` +
            `<td><span class="chip ${c.bloom ? "" : "off"}">${c.bloom ? "bloom" : "—"}</span></td></tr>`
        )
        .join("");
      return (
        `<div class="rg${verdictClass(verdicts[i])}"><div class="rg-head">` +
        `<span><b>row group ${i}</b> · ${rg.rows.toLocaleString()} rows · ${humanBytes(rg.bytes)}` +
        verdictChip(verdicts[i]) +
        `</span>` +
        `<span style="color:var(--dim)">${rg.resident === null ? "resident" : `${rg.resident}/${rg.columns.length} columns decoded`}</span></div>` +
        `<table class="zone"><thead><tr><th>column</th><th>type</th><th>min</th><th>max</th><th>nulls</th><th>distinct</th><th>encoding</th><th></th></tr></thead>` +
        `<tbody>${rows}</tbody></table></div>`
      );
    })
    .join("");

  body.innerHTML =
    head +
    `<p class="tree-legend">min and max are the zone map: a predicate outside a group's range skips it without reading a byte. ` +
    `For a Parquet table both come from the file's footer, and the decoded count is what the queries you have run actually needed.` +
    (verdicts.length > 0
      ? ` The last query's scan of this table skipped <b>${skipped}</b> of ${verdicts.length} ` +
        `row group${verdicts.length === 1 ? "" : "s"}, and each one below says what skipped it.`
      : ` Run a query against this table and the groups below will say which were read and which were skipped.`) +
    `</p>` +
    groups +
    (info.row_groups.length > ROW_GROUPS_SHOWN
      ? `<p class="empty">showing ${ROW_GROUPS_SHOWN} of ${info.row_groups.length} row groups. ` +
        `The rest look like these: same columns, their own bounds.</p>`
      : "");
}

function tableSwitcher(current: string, onPick: (name: string) => void): string {
  const tables = JSON.parse(engine.tables()) as TableInfo[];
  const id = `switch-${Math.random().toString(36).slice(2)}`;
  setTimeout(() => {
    const select = document.getElementById(id) as HTMLSelectElement | null;
    if (select) select.addEventListener("change", () => onPick(select.value));
  }, 0);
  return (
    `<select id="${id}">` +
    tables
      .map(
        (t: TableInfo) =>
          `<option value="${escapeHtml(t.name)}"${t.name === current ? " selected" : ""}>${escapeHtml(t.name)}</option>`
      )
      .join("") +
    `</select>`
  );
}

// ---------------------------------------------------------------------------
// Index visualizer
// ---------------------------------------------------------------------------


/**
 * Draw a B+ tree, and the path a probe took down it.
 *
 * The tree is sampled: a level wider than the cap is shown evenly spaced across
 * its key range, with the nodes on the traversal path always kept. Drawing
 * every node of an index over a million rows would be tens of thousands of
 * rectangles and no more informative.
 */
function renderIndex() {
  const body = $("tab-index");
  const tables = JSON.parse(engine.tables()) as TableInfo[];
  const indexed = tables.flatMap((t) =>
    t.indexes.map((c) => ({ table: t.name, column: c }))
  );

  if (indexed.length === 0) {
    body.innerHTML =
      `<p class="empty">no indexes yet. Build one and this draws its B+ tree:</p>` +
      `<div class="tree-controls">` +
      tables
        .map(
          (t) =>
            t.columns
              .map(
                (c) =>
                  `<button class="mk-index" data-t="${escapeHtml(t.name)}" data-c="${escapeHtml(c.name)}">${escapeHtml(t.name)}.${escapeHtml(c.name)}</button>`
              )
              .join("")
        )
        .join("") +
      `</div>`;
    for (const button of body.querySelectorAll<HTMLElement>(".mk-index")) {
      const table = button.dataset.t!;
      const column = button.dataset.c!;
      button.addEventListener("click", () => {
        engine.createIndex(table, column);
        indexChoice = { table, column };
        renderIndex();
      });
    }
    return;
  }

  // Keep the selection when the index behind it still exists -- rendering the
  // tree must not silently move the reader to a different one -- and otherwise
  // fall back to the first. Bound to a local because `indexChoice` is
  // module-level, and the closures below need it to stay non-null.
  const previous = indexChoice;
  const choice =
    previous && indexed.some((i) => i.table === previous.table && i.column === previous.column)
      ? previous
      : indexed[0];
  indexChoice = choice;
  const probe = body.querySelector<HTMLInputElement>("#probe")?.value ?? "";

  let info: IndexInfo;
  let error: string | null = null;
  try {
    info = JSON.parse(engine.indexTree(choice.table, choice.column, probe || undefined, 9));
  } catch (e) {
    error = message(e);
    info = JSON.parse(engine.indexTree(choice.table, choice.column, undefined, 9));
  }

  body.innerHTML =
    `<div class="tree-controls">` +
    `<select id="index-pick">` +
    indexed
      .map(
        (i) =>
          `<option value="${escapeHtml(i.table)}|${escapeHtml(i.column)}"${i.table === choice.table && i.column === choice.column ? " selected" : ""}>${escapeHtml(i.table)}.${escapeHtml(i.column)}</option>`
      )
      .join("") +
    `</select>` +
    `<input type="text" id="probe" placeholder="look up a key…" value="${escapeHtml(probe)}">` +
    `<button id="probe-go">descend</button>` +
    `<span class="slider-label">${info.keys.toLocaleString()} keys · ${info.entries.toLocaleString()} rows · ` +
    `${info.height} level${info.height === 1 ? "" : "s"} · ${info.nodes.toLocaleString()} nodes · ${(info.bytes / 1024).toFixed(0)} KiB</span>` +
    `</div>` +
    (error ? `<div class="error">${escapeHtml(error)}</div>` : "") +
    treeSvg(info) +
    `<p class="tree-legend">` +
    (info.path.length
      ? `the descent visited ${info.path.length} node${info.path.length === 1 ? "" : "s"}, one per level, and found ` +
        `<b>${info.matched}</b> row${info.matched === 1 ? "" : "s"}. Values live only in the leaves; the levels above route.`
      : `type a key and descend to see the path taken. Values live only in the leaves; the levels above route, and the leaves are chained left to right so a range scan descends once and then walks.`) +
    `</p>`;

  const pick = $<HTMLSelectElement>("index-pick");
  pick.addEventListener("change", () => {
    const [table, column] = pick.value.split("|");
    indexChoice = { table, column };
    renderIndex();
  });
  $("probe-go").addEventListener("click", renderIndex);
  $("probe").addEventListener("keydown", (e) => {
    if (e.key === "Enter") renderIndex();
  });
}

function treeSvg(info: IndexInfo): string {
  const width = 900;
  const rowHeight = 78;
  const height = info.height * rowHeight + 24;
  const byLevel: TreeNodeInfo[][] = [];
  for (const node of info.tree) (byLevel[node.level] ??= []).push(node);

  interface Box {
    x: number;
    y: number;
    w: number;
    h: number;
    node: TreeNodeInfo;
  }
  const place = new Map<number, Box>();
  byLevel.forEach((nodes, level) => {
    const boxWidth = Math.min(120, (width - 20) / nodes.length - 8);
    const gap = (width - 20 - boxWidth * nodes.length) / Math.max(1, nodes.length - 1 || 1);
    nodes.forEach((node, i) => {
      const x = nodes.length === 1 ? (width - boxWidth) / 2 : 10 + i * (boxWidth + gap);
      place.set(node.id, { x, y: level * rowHeight + 16, w: boxWidth, h: 34, node });
    });
  });

  const edges: string[] = [];
  for (const node of info.tree) {
    const from = place.get(node.id);
    if (!from) continue;
    for (const child of node.children) {
      const to = place.get(child);
      if (!to) continue;
      const onPath = node.on_path && info.path.includes(child);
      edges.push(
        `<path class="edge${onPath ? " on-path" : ""}" d="M${from.x + from.w / 2},${from.y + from.h} C${from.x + from.w / 2},${from.y + from.h + 20} ${to.x + to.w / 2},${to.y - 20} ${to.x + to.w / 2},${to.y}"/>`
      );
    }
  }

  const boxes = [...place.values()].map(({ x, y, w, h, node }) => {
    // Roughly 5px per character at this font size. A box too narrow for the key
    // range shows only its fill, and the range moves into the tooltip -- two
    // labels overlapping is worse than one label missing.
    const budget = Math.floor(w / 5.2);
    const range = node.first === null ? "empty" : `${node.first}…${node.last}`;
    const label = budget >= 8 ? fit(range, budget) : "";
    const count = `${node.keys} key${node.keys === 1 ? "" : "s"}`;
    return (
      `<g class="${node.on_path ? "on-path" : ""}"><title>${escapeHtml(range)} · ${count}</title>` +
      `<rect class="node${node.leaf ? " leaf" : ""}${node.on_path ? " on-path" : ""}" x="${x}" y="${y}" width="${w}" height="${h}" rx="4"/>` +
      (label
        ? `<text x="${x + w / 2}" y="${y + 14}" text-anchor="middle" class="${node.on_path ? "on-path" : ""}">${escapeHtml(label)}</text>`
        : "") +
      `<text x="${x + w / 2}" y="${label ? y + 26 : y + 21}" text-anchor="middle">${count}</text>` +
      `</g>`
    );
  });

  // Placed above its row rather than beside it, where it would sit on top of
  // the right-hand node.
  const notes = byLevel
    .map((nodes, level) =>
      nodes[0] && nodes[0].level_total > nodes.length
        ? `<text x="${width - 6}" y="${level * rowHeight + 6}" text-anchor="end">showing ${nodes.length} of ${nodes[0].level_total} nodes on this level</text>`
        : ""
    )
    .join("");

  return `<svg class="tree-svg" viewBox="0 0 ${width} ${height}" preserveAspectRatio="xMidYMin meet">${edges.join("")}${boxes.join("")}${notes}</svg>`;
}

/** Squeeze a key range into `budget` characters, keeping both ends visible. */
function fit(text: string, budget: number): string {
  if (text.length <= budget) return text;
  const half = Math.max(2, Math.floor((budget - 1) / 2));
  return `${text.slice(0, half)}…${text.slice(-half)}`;
}

function escapeHtml(s: unknown): string {
  const map: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;" };
  return String(s).replace(/[&<>]/g, (c) => map[c]);
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/**
 * An example query, and what to watch while it runs.
 *
 * `watch` is the point of the list. Any of these could be typed by hand; what
 * an example is *for* is knowing which tab to open and which line to look for
 * once the answer is on screen, and a query with no such note is just a query.
 */
interface Example {
  label: string;
  sql: string;
  watch: string;
}

/** The output panels, in the order the pipeline produces them. */
const TABS = [
  "results",
  "tokens",
  "ast",
  "plan",
  "trace",
  "physical",
  "pipeline",
  "storage",
  "index",
];

const EXAMPLES: Example[] = [
  {
    label: "a filter and a sort",
    sql: "SELECT name, city, salary\nFROM people\nWHERE salary > 170000\nORDER BY salary DESC",
    watch:
      "optimizer trace → predicate_pushdown moves the filter under the sort, so the sort orders the rows that survive rather than all of them.",
  },
  {
    label: "a join",
    sql: "SELECT p.name, o.product, o.quantity * o.unit_price AS total\nFROM people p JOIN purchases o ON p.id = o.person_id\nORDER BY total DESC\nLIMIT 5",
    watch:
      "physical plan → the sort is a TopN, not a Sort: five rows were asked for, so only five are ever kept.",
  },
  {
    label: "grouping",
    sql: "SELECT department, COUNT(*) AS n, AVG(salary) AS avg_salary\nFROM people\nGROUP BY department\nORDER BY n DESC",
    watch:
      "physical plan → HashAggregate, and its reason: the input is not sorted on department, so grouping hashes rather than streams.",
  },
  {
    label: "a window function",
    sql: "SELECT name, department, salary,\n       RANK() OVER (PARTITION BY department ORDER BY salary DESC) AS rank\nFROM people",
    watch:
      "pipeline → the sort feeding the window is the whole cost of a window function; the ranking itself is one pass.",
  },
  {
    label: "three-valued logic",
    sql: "SELECT name, score, score > 9 AS high\nFROM people\nWHERE score IS NULL OR score > 9",
    watch:
      "results → `high` is NULL, not false, where `score` is NULL. A comparison with NULL is UNKNOWN, and the filter keeps a row only when its predicate is TRUE.",
  },
  {
    label: "a correlated subquery",
    sql: "SELECT name FROM people p\nWHERE EXISTS (SELECT 1 FROM purchases o WHERE o.person_id = p.id AND o.quantity > 2)",
    watch:
      "optimizer trace → decorrelate rewrites the EXISTS into a semi join, so the subquery runs once rather than once per row.",
  },
  {
    label: "set operations",
    sql: "SELECT city FROM people WHERE salary > 180000\nEXCEPT\nSELECT city FROM people WHERE department = 'Research'",
    watch:
      "physical plan → EXCEPT builds a hash set from the right side and probes it with the left, and deduplicates on the way out.",
  },
  {
    label: "a contradiction",
    sql: "SELECT name, salary FROM people\nWHERE salary > 200000 AND salary < 100000",
    watch:
      "optimizer trace → predicate_simplification proves the two bounds cannot both hold and folds the whole query to an empty result. The scan never runs.",
  },
  {
    label: "an outer join that is not one",
    sql: "SELECT p.name, o.product\nFROM people p LEFT JOIN purchases o ON p.id = o.person_id\nWHERE o.quantity > 2",
    watch:
      "optimizer trace → outer_to_inner. The filter rejects the NULLs the LEFT join manufactures, so the outerness was doing nothing and the join becomes inner.",
  },
  {
    label: "a repeated expression",
    sql: "SELECT name,\n       salary * 1.1 AS raised,\n       salary * 1.1 - salary AS increase\nFROM people\nWHERE salary * 1.1 > 180000",
    watch:
      "optimizer trace → common_subexpression hoists `salary * 1.1` into a project below, computed once and read three times.",
  },
  {
    label: "an error, with its caret",
    sql: "SELECT naem FROM people",
    watch:
      "the editor → the unknown column is underlined where it appears, from the byte offsets the binder attaches to every name.",
  },
];

/**
 * The examples on offer, growing as collections arrive.
 *
 * A copy rather than `EXAMPLES` itself, so the built-in list stays the list of
 * things that work against the tables the page ships with.
 */
const examples: Example[] = [...EXAMPLES];

/** Rebuild the example dropdown, keeping whatever was selected selected. */
function renderExamples() {
  const select = $<HTMLSelectElement>("examples");
  const chosen = select.value;
  select.innerHTML = examples
    .map((e, i) => `<option value="${i}">${escapeHtml(e.label)}</option>`)
    .join("");
  if (chosen) select.value = chosen;
}

// ---------------------------------------------------------------------------
// Share links
// ---------------------------------------------------------------------------

/**
 * The query and dataset in the address bar, if there are any.
 *
 * A link to this page is worth nothing if it always opens on the same example,
 * so the query travels in the URL. `dataset` names which table the storage
 * inspector opens on -- every dataset is loaded either way, so it selects a
 * view rather than loading anything.
 */
function fromUrl(): { query: string | null; dataset: string | null } {
  const params = new URLSearchParams(location.search);
  return { query: params.get("query"), dataset: params.get("dataset") };
}

/** Put the current query in the address bar and on the clipboard. */
async function share() {
  const url = new URL(location.href);
  url.searchParams.set("query", editor.value);
  url.searchParams.set("dataset", storageTable);
  // Replaced rather than pushed: the back button should leave the page, not
  // walk backwards through every query typed into it.
  history.replaceState(null, "", url);

  const status = $("share-status");
  try {
    await navigator.clipboard.writeText(url.toString());
    status.textContent = "link copied";
  } catch {
    // A denied clipboard permission is not a failure worth an error banner --
    // the address bar now holds the link either way.
    status.textContent = "link is in the address bar";
  }
  setTimeout(() => {
    status.textContent = "";
  }, 4000);
}


async function boot() {
  wasm = await init();
  engine = new QueryEngine();

  const shared = fromUrl();
  if (shared.dataset) storageTable = shared.dataset;

  editor = new SqlEditor({
    parent: $("editor"),
    initial: shared.query ?? EXAMPLES[0].sql,
    onRun: run,
    // Re-parsed on every keystroke to place the underline. Cheap: it stops
    // before the optimizer, which is the part that costs anything.
    check: (text): SpanError | null => {
      const raw = engine.check(text);
      if (raw === "null") return null;
      const info = JSON.parse(raw) as CheckInfo;
      return {
        message: info.hint ? `${info.message} — ${info.hint}` : info.message,
        start: info.start,
        end: info.end,
      };
    },
    tables: () =>
      (JSON.parse(engine.tables()) as TableInfo[]).map((t) => ({
        name: t.name,
        columns: t.columns.map((c) => ({ name: c.name, type: c.type })),
      })),
  });

  const select = $<HTMLSelectElement>("examples");
  renderExamples();
  select.addEventListener("change", () => {
    const example = examples[Number(select.value)];
    editor.value = example.sql;
    $("watch").textContent = example.watch;
    run();
  });
  // A shared link opens on its own query, which is nobody's example.
  if (shared.query) $("watch").textContent = "";
  else $("watch").textContent = examples[0].watch;

  $("share").addEventListener("click", () => {
    void share();
  });

  const db = await openDb();
  let cachedCount = 0;
  for (const dataset of DATASETS) {
    const { bytes, cached } = await fetchDataset(dataset.url, db);
    if (cached) cachedCount++;
    engine.load(dataset.name, bytes);
  }
  $("load-status").textContent =
    cachedCount > 0 ? `${cachedCount} from IndexedDB` : "fetched";
  renderCatalog();
  // The editor was built before any of this existed. Only now is there a
  // schema for `lang-sql` to complete qualified names against.
  editor.refreshSchema();

  $("run").addEventListener("click", run);
  for (const tab of document.querySelectorAll<HTMLElement>(".tab")) {
    tab.addEventListener("click", () => {
      document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t === tab));
      for (const name of TABS) {
        $(`tab-${name}`).hidden = name !== tab.dataset.tab;
      }
      // Built when opened rather than after every query: neither depends on the
      // last result, and drawing a tree nobody is looking at is waste.
      if (tab.dataset.tab === "storage") renderStorage(storageTable);
      if (tab.dataset.tab === "index") renderIndex();
    });
  }

  // A HEAD to a compressed response reports the *transfer* size, which is the
  // honest number for a page to quote but is not the module's size. Say which.
  $("build").textContent =
    `wasm module ${humanBytes(wasmSize.bytes)}${wasmSize.compressed ? " over the wire" : ""}` +
    ` · engine built from scratch, zero dependencies`;
  run();
}

function renderCatalog() {
  const tables = JSON.parse(engine.tables()) as TableInfo[];
  $("catalog").innerHTML = tables
    .map(
      (t) =>
        `<div class="tablecard" data-name="${escapeHtml(t.name)}"><b>${escapeHtml(t.name)}</b> ` +
        `<span>${t.rows.toLocaleString()} rows · ${t.columns.length} cols · ${t.row_groups} row group${t.row_groups === 1 ? "" : "s"}` +
        (t.pending ? " · lazily decoded" : "") +
        `</span><br><span>${t.columns
          .map((c) => `${escapeHtml(c.name)} ${c.type.toLowerCase()}`)
          .join(", ")}</span></div>`
    )
    .join("");
  for (const card of document.querySelectorAll<HTMLElement>(".tablecard")) {
    card.addEventListener("click", () => {
      editor.value = `SELECT * FROM ${card.dataset.name} LIMIT 20`;
      run();
    });
  }
  renderCollections();
}

/**
 * The buttons that fetch the real datasets.
 *
 * Redrawn with the catalog because a loaded collection has to stop offering
 * itself, and because the counts above it change the moment one arrives.
 */
function renderCollections() {
  const row = $("collections");
  const pending = COLLECTIONS.filter((c) => !loaded.has(c.id));
  if (pending.length === 0) {
    row.innerHTML = "";
    return;
  }
  row.innerHTML =
    `<span class="hint">fetch from a CDN, cached in IndexedDB:</span>` +
    pending
      .map(
        (c) =>
          `<button class="load-set" data-id="${c.id}" title="${escapeHtml(c.blurb)}">` +
          `${escapeHtml(c.label)} <span class="dim">${escapeHtml(c.size)}</span></button>`
      )
      .join("");

  for (const button of row.querySelectorAll<HTMLButtonElement>(".load-set")) {
    button.addEventListener("click", () => {
      const collection = COLLECTIONS.find((c) => c.id === button.dataset.id);
      if (collection) void loadCollection(collection, button);
    });
  }
}

/**
 * Fetch a collection's tables and add them to the catalog.
 *
 * Sequential rather than parallel: eight parallel fetches of a few hundred
 * kilobytes would be fine, but one parallel fetch of 127 MB alongside them is
 * not, and one progress line that means something beats eight that fight over
 * it.
 */
async function loadCollection(collection: Collection, button: HTMLButtonElement) {
  button.disabled = true;
  const say = (text: string) => {
    button.innerHTML = `${escapeHtml(collection.label)} <span class="dim">${escapeHtml(text)}</span>`;
  };

  try {
    // Noted before the load, so a replaced table can be named afterwards.
    const existing = (JSON.parse(engine.tables()) as TableInfo[]).map((t) => t.name);
    const db = await openDb();
    for (const [i, table] of collection.tables.entries()) {
      const of =
        collection.tables.length > 1 ? ` (${i + 1}/${collection.tables.length})` : "";
      say(`fetching${of}…`);
      const { bytes, cached } = await fetchDataset(table.url, db, (received, total) => {
        const mb = (received / 1048576).toFixed(0);
        say(total > 0 ? `${((received / total) * 100).toFixed(0)}%${of}` : `${mb} MB${of}`);
      });
      say(cached ? `from cache${of}` : `loading${of}…`);
      // Yield so the button's last state paints before the engine takes the
      // thread to parse a footer.
      await new Promise((r) => setTimeout(r, 0));
      engine.load(table.name, bytes);
    }
    const clashes = existing.filter((name) =>
      collection.tables.some((t) => t.name === name)
    );
    if (clashes.length > 0) {
      const banner = $("error");
      banner.hidden = false;
      banner.textContent =
        `${collection.label} replaced the table${clashes.length === 1 ? "" : "s"} ` +
        `${clashes.join(", ")}. Queries written against the old one will not bind.`;
    }
    loaded.add(collection.id);
    examples.push(...collection.examples);
    renderExamples();
    renderCatalog();
    // New tables, so completion has more to offer and the storage inspector
    // has somewhere new to point.
    editor.refreshSchema();
  } catch (e) {
    say("failed");
    const banner = $("error");
    banner.hidden = false;
    banner.textContent = `${collection.label}: ${message(e)}`;
    button.disabled = false;
  }
}

/** Rows the grid will draw. The engine computes every row either way. */
const RESULT_CAP = 10_000;

/**
 * Rows a table can hold before a query is run a batch at a time.
 *
 * The engine runs on the main thread, so a query that takes a second takes the
 * tab with it: no progress, no repaint, no way to tell a slow query from a
 * hung one. Streaming hands back a batch at a time and this yields to the
 * event loop between them, which buys a row counter that moves.
 *
 * Small tables go the whole-result route because at three rows the counter
 * would flash once and the yields would be the slowest part of the query.
 */
const STREAM_ABOVE_ROWS = 200_000;

/** Set while a query is running, so a second Run cannot interleave with it. */
let running = false;

/**
 * Yield to the event loop, without the throttling `setTimeout` carries.
 *
 * A nested `setTimeout(0)` is clamped to 4ms after a few levels, and in a
 * backgrounded tab to a *second* -- which turns a streamed query into one that
 * never finishes. A message posted to a channel is a task like any other and
 * is not clamped, so it yields exactly as long as it takes the browser to
 * paint.
 */
const yieldToBrowser = (() => {
  const channel = new MessageChannel();
  let resolve: (() => void) | null = null;
  channel.port1.onmessage = () => {
    const r = resolve;
    resolve = null;
    r?.();
  };
  return () =>
    new Promise<void>((r) => {
      resolve = r;
      channel.port2.postMessage(null);
    });
})();

/**
 * How long to work before yielding.
 *
 * One yield per batch would be one yield per 2048 rows, which on a
 * seven-million-row scan is three thousand round trips through the event loop
 * for sixty useful repaints. Sixteen milliseconds is a frame: the counter
 * still moves smoothly and the yields stop being the slowest part.
 */
const FRAME_MS = 16;

function run() {
  void runQuery();
}

async function runQuery() {
  const sql = editor.value.trim();
  if (!sql || running) return;
  const error = $("error");
  error.hidden = true;
  running = true;
  $<HTMLButtonElement>("run").disabled = true;

  try {
    const biggest = (JSON.parse(engine.tables()) as TableInfo[]).reduce(
      (n, t) => Math.max(n, t.rows),
      0
    );
    const meta = biggest >= STREAM_ABOVE_ROWS ? await runStreaming(sql) : runWhole(sql);
    if (!meta) return;

    renderPipeline(meta.stats);
    $("timing").textContent =
      `${meta.num_rows.toLocaleString()} row${meta.num_rows === 1 ? "" : "s"} in ${meta.elapsed_ms.toFixed(3)} ms`;

    try {
      const explain = JSON.parse(engine.explain(sql));
      renderTokens(explain);
      renderAst(explain);
      renderPlan(explain);
      renderTrace(explain);
      renderPhysical(sql);
    } catch {
      // A query can execute and still not re-plan (it cannot, in practice) --
      // but the results are already rendered, so a plan failure must not lose
      // them.
    }
  } catch (e) {
    error.hidden = false;
    error.textContent = message(e);
    $("tab-results").textContent = "";
    $("timing").textContent = "";
  } finally {
    running = false;
    $<HTMLButtonElement>("run").disabled = false;
  }
}

/** The whole result at once: one call, one render. */
function runWhole(sql: string): OutcomeMeta {
  const outcome = engine.query(sql);
  const meta = JSON.parse(outcome.meta) as OutcomeMeta;
  renderResults(outcome, meta);
  return meta;
}

/**
 * A batch at a time, drawing rows and a count as they arrive.
 *
 * The per-chunk `meta` carries the schema and the stats accumulated so far, so
 * the last one seen is the whole query's. `num_rows` on a chunk is that
 * chunk's, which is why the running total is counted here.
 */
async function runStreaming(sql: string): Promise<OutcomeMeta | null> {
  const body = $("tab-results");
  body.textContent = "";
  const timing = $("timing");

  const progress = engine.stream(sql);
  let table: HTMLTableElement | null = null;
  let shown = 0;
  let total = 0;
  // The schema, read from the first chunk. Every later chunk carries the same
  // one, and its `meta` also carries the whole operator-statistics tree --
  // serialized afresh on each read, so reading it per batch would cost more
  // than the batch did.
  let schema: OutcomeMeta | null = null;
  let lastMeta: OutcomeMeta | null = null;
  let frameStart = performance.now();

  for (;;) {
    const chunk = progress.next();
    if (!chunk) break;

    if (!schema) {
      schema = JSON.parse(chunk.meta) as OutcomeMeta;
      lastMeta = schema;
      if (schema.columns.length > 0) {
        table = resultTable(schema);
        body.appendChild(table);
      }
    }
    total += chunk.len(0);
    if (table && schema && shown < RESULT_CAP) {
      shown += appendRows(table, chunk, schema, RESULT_CAP - shown);
    }

    if (performance.now() - frameStart >= FRAME_MS) {
      timing.textContent = `${total.toLocaleString()} rows…`;
      // The execution panel, while the query is still running: rows climbing
      // through each operator, the time bars redistributing, the estimate
      // sitting still beside a number that is not. Only when it is on screen
      // -- the statistics tree is not free to read, and redrawing a hidden
      // panel sixty times a second is the kind of cost that makes streaming
      // slower than not streaming.
      if (!$("tab-pipeline").hidden) {
        renderPipeline(JSON.parse(progress.stats) as StatsInfo);
      }
      await yieldToBrowser();
      frameStart = performance.now();
    }
  }

  // Once, at the end, for the statistics the execution panel draws.
  const last = lastMeta;
  if (!last) return null;
  if (total === 0) {
    body.innerHTML = '<p class="empty">no rows</p>';
  } else if (shown < total) {
    const note = document.createElement("p");
    note.className = "empty";
    note.textContent = `showing the first ${shown.toLocaleString()} of ${total.toLocaleString()} rows — the query computed all of them`;
    body.appendChild(note);
  }

  // The stream's own totals, so the timing line reports the query and not its
  // last batch. The statistics tree is re-read here too: it accumulates as the
  // stream runs, so only the final read describes the whole query.
  const done = JSON.parse(progress.progress) as { rows: number; elapsed_ms: number };
  const final = JSON.parse(progress.stats) as OutcomeMeta["stats"];
  return { ...last, stats: final, num_rows: done.rows, elapsed_ms: done.elapsed_ms };
}

fetch(wasmUrl, { method: "HEAD" })
  .then((r) => {
    wasmSize = {
      bytes: Number(r.headers.get("content-length") ?? 0),
      compressed: r.headers.get("content-encoding") !== null,
    };
  })
  .catch(() => {});

boot().catch((e) => {
  const banner = document.getElementById("error");
  if (!banner) return;
  banner.hidden = false;
  banner.textContent = `failed to start: ${message(e)}`;
});
