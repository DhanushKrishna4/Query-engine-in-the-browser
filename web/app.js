// The page. Everything below the boundary is Rust; this file only fetches
// data, hands buffers in, and reads buffers out.
import init, { QueryEngine, ColumnKind } from "./pkg/qe.js";

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

// Small enough to ship beside the page. Anything larger belongs on a CDN and
// is fetched by URL -- the engine never does I/O itself, so a dataset is just
// a byte buffer that arrived from somewhere.
const DATASETS = [
  { name: "people", url: "data/people.csv" },
  { name: "orders", url: "data/orders.csv" },
  // Parquet, eight row groups: the table the storage inspector is worth
  // opening on, because its zone maps come from the file's own footer and its
  // columns are decoded only when a scan asks.
  { name: "metrics", url: "data/metrics.parquet" },
];

const DB_NAME = "qe-cache";
const STORE = "datasets";

/** Open the IndexedDB cache, or resolve to null if the browser refuses. */
function openDb() {
  return new Promise((resolve) => {
    let request;
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

function idb(db, mode, fn) {
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
async function fetchDataset(url, db) {
  if (db) {
    const cached = await idb(db, "readonly", (store) => store.get(url));
    if (cached) return { bytes: new Uint8Array(cached), cached: true };
  }
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status} ${response.statusText}`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (db) await idb(db, "readwrite", (store) => store.put(bytes.buffer, url));
  return { bytes, cached: false };
}

// ---------------------------------------------------------------------------
// Reading results out of wasm memory
// ---------------------------------------------------------------------------

let wasm; // the module's exports, for `wasm.memory`

/**
 * Build typed-array views onto a result column, without copying it.
 *
 * `memory.buffer` is re-read on every call and no view outlives this function,
 * because allocating inside wasm can grow the memory and growing detaches every
 * ArrayBuffer built on the old one.
 */
function columnViews(outcome, index) {
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

function cell(view, row) {
  if (view.validity && (view.validity[row >> 3] & (1 << (row & 7))) === 0) return null;
  switch (view.kind) {
    case ColumnKind.Boolean:
      return (view.values[row >> 3] & (1 << (row & 7))) !== 0;
    case ColumnKind.Int32:
    case ColumnKind.Float64:
      return view.values[row];
    case ColumnKind.Int64:
      return view.values[row];
    case ColumnKind.Date32:
      return new Date(view.values[row] * MS_PER_DAY).toISOString().slice(0, 10);
    case ColumnKind.Timestamp:
      // Microseconds since the epoch. Milliseconds is all a JS Date holds, so
      // the sub-millisecond part is printed separately rather than dropped.
      return new Date(Number(view.values[row] / 1000n)).toISOString().replace("T", " ").replace("Z", "");
    case ColumnKind.Utf8:
      return decoder.decode(view.values.subarray(view.offsets[row], view.offsets[row + 1]));
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);

function renderResults(outcome, meta) {
  const body = $("tab-results");
  body.textContent = "";
  if (meta.columns.length === 0) {
    body.innerHTML = '<p class="empty">no columns</p>';
    return;
  }

  const views = meta.columns.map((_, i) => columnViews(outcome, i));
  const rows = views[0]?.rows ?? 0;

  const table = document.createElement("table");
  const head = table.createTHead().insertRow();
  for (const column of meta.columns) {
    const th = document.createElement("th");
    th.textContent = column.name;
    th.title = column.type + (column.nullable ? " (nullable)" : "");
    head.appendChild(th);
  }
  const tbody = table.createTBody();
  for (let row = 0; row < rows; row++) {
    const tr = tbody.insertRow();
    for (let c = 0; c < views.length; c++) {
      const td = tr.insertCell();
      const value = cell(views[c], row);
      if (value === null) {
        td.className = "null";
        td.textContent = "NULL";
      } else {
        const numeric = typeof value === "number" || typeof value === "bigint";
        if (numeric) td.className = "num";
        td.textContent = String(value);
      }
    }
  }
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
function renderPipeline(stats) {
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

function renderPlan(explain) {
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
function renderPhysical(sql) {
  const body = $("tab-physical");
  let root;
  try {
    root = JSON.parse(engine.physicalPlan(sql));
  } catch (e) {
    body.innerHTML = `<div class="error">${escapeHtml(String(e.message ?? e))}</div>`;
    return;
  }
  const lines = [];
  (function walk(node, depth) {
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
    node.children.forEach((c) => walk(c, depth + 1));
  })(root, 0);
  body.innerHTML = `<div class="phys">${lines.join("")}</div>`;
}

/** Collect the rel ids in a subtree, so the whole of it can be highlighted. */
function subtreeRels(node, target, found = new Set(), inside = false) {
  const here = inside || node.rel === target;
  if (here) found.add(node.rel);
  for (const child of node.children) subtreeRels(child, target, found, here);
  return found;
}

/** Render a plan as an indented tree, marking the subtree a rule fired at. */
function renderPlanTree(root, target) {
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

let traceSteps = [];

/**
 * The optimizer trace, one rewrite at a time.
 *
 * Each step names the node the rule fired at, so the subtree it touched is
 * highlighted rather than left to be found by diffing two blocks of text.
 */
function renderTrace(explain) {
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

  const slider = $("step-slider");
  const show = (n) => {
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

/** Which row groups the last query's scan actually read, by table. */
let lastScans = new Map();

function collectScans(node, into) {
  if (node.row_groups_total > 0) {
    into.set(node.detail.match(/table=(\w+)/)?.[1] ?? "", {
      scanned: node.row_groups_scanned,
      pruned: node.row_groups_pruned,
    });
  }
  node.children.forEach((child) => collectScans(child, into));
  return into;
}

function renderStorage(table) {
  storageTable = table;
  const body = $("tab-storage");
  let info;
  try {
    info = JSON.parse(engine.storage(table));
  } catch (e) {
    body.innerHTML = `<p class="empty">${escapeHtml(String(e.message ?? e))}</p>`;
    return;
  }

  const head =
    `<div class="tree-controls"><span class="slider-label">` +
    `<b>${escapeHtml(info.table)}</b> · ${info.rows.toLocaleString()} rows · ` +
    `${info.row_groups.length} row group${info.row_groups.length === 1 ? "" : "s"} · ` +
    `${(info.bytes / 1024).toFixed(1)} KiB` +
    (info.pending ? " · lazily decoded from Parquet" : " · fully resident") +
    `</span>` +
    tableSwitcher(table, renderStorage) +
    `</div>`;

  const groups = info.row_groups
    .map((rg, i) => {
      const rows = rg.columns
        .map(
          (c) =>
            `<tr><td>${escapeHtml(c.name)}</td><td style="color:var(--dim)">${escapeHtml(c.type.toLowerCase())}</td>` +
            `<td>${c.min === null ? "—" : escapeHtml(c.min)}</td><td>${c.max === null ? "—" : escapeHtml(c.max)}</td>` +
            `<td class="num">${c.nulls}</td>` +
            `<td class="num">${c.distinct === null ? "&gt;8192" : c.distinct}</td>` +
            `<td><span class="chip ${c.bloom ? "" : "off"}">${c.bloom ? "bloom" : "—"}</span></td></tr>`
        )
        .join("");
      return (
        `<div class="rg"><div class="rg-head"><span><b>row group ${i}</b> · ${rg.rows.toLocaleString()} rows · ${(rg.bytes / 1024).toFixed(1)} KiB</span>` +
        `<span style="color:var(--dim)">${rg.resident === null ? "resident" : `${rg.resident}/${rg.columns.length} columns decoded`}</span></div>` +
        `<table class="zone"><thead><tr><th>column</th><th>type</th><th>min</th><th>max</th><th>nulls</th><th>distinct</th><th></th></tr></thead>` +
        `<tbody>${rows}</tbody></table></div>`
      );
    })
    .join("");

  body.innerHTML =
    head +
    `<p class="tree-legend">min and max are the zone map: a predicate outside a group's range skips it without reading a byte. ` +
    `For a Parquet table both come from the file's footer, and the decoded count is what the queries you have run actually needed.</p>` +
    groups;
}

function tableSwitcher(current, onPick) {
  const tables = JSON.parse(engine.tables());
  const id = `switch-${Math.random().toString(36).slice(2)}`;
  setTimeout(() => {
    const select = document.getElementById(id);
    if (select) select.addEventListener("change", () => onPick(select.value));
  }, 0);
  return (
    `<select id="${id}">` +
    tables
      .map((t) => `<option value="${escapeHtml(t.name)}"${t.name === current ? " selected" : ""}>${escapeHtml(t.name)}</option>`)
      .join("") +
    `</select>`
  );
}

// ---------------------------------------------------------------------------
// Index visualizer
// ---------------------------------------------------------------------------

let indexChoice = null;

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
  const tables = JSON.parse(engine.tables());
  const indexed = tables.flatMap((t) => t.indexes.map((c) => ({ table: t.name, column: c })));

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
    for (const button of body.querySelectorAll(".mk-index")) {
      button.addEventListener("click", () => {
        engine.createIndex(button.dataset.t, button.dataset.c);
        indexChoice = { table: button.dataset.t, column: button.dataset.c };
        renderCatalog();
        renderIndex();
      });
    }
    return;
  }

  if (!indexChoice || !indexed.some((i) => i.table === indexChoice.table && i.column === indexChoice.column)) {
    indexChoice = indexed[0];
  }
  const probe = body.querySelector("#probe")?.value ?? "";

  let info;
  let error = null;
  try {
    info = JSON.parse(engine.indexTree(indexChoice.table, indexChoice.column, probe || undefined, 9));
  } catch (e) {
    error = String(e.message ?? e);
    info = JSON.parse(engine.indexTree(indexChoice.table, indexChoice.column, undefined, 9));
  }

  body.innerHTML =
    `<div class="tree-controls">` +
    `<select id="index-pick">` +
    indexed
      .map(
        (i) =>
          `<option value="${escapeHtml(i.table)}|${escapeHtml(i.column)}"${i.table === indexChoice.table && i.column === indexChoice.column ? " selected" : ""}>${escapeHtml(i.table)}.${escapeHtml(i.column)}</option>`
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

  $("index-pick").addEventListener("change", (e) => {
    const [table, column] = e.target.value.split("|");
    indexChoice = { table, column };
    renderIndex();
  });
  $("probe-go").addEventListener("click", renderIndex);
  $("probe").addEventListener("keydown", (e) => {
    if (e.key === "Enter") renderIndex();
  });
}

function treeSvg(info) {
  const width = 900;
  const rowHeight = 78;
  const height = info.height * rowHeight + 24;
  const byLevel = [];
  for (const node of info.tree) (byLevel[node.level] ??= []).push(node);

  const place = new Map();
  byLevel.forEach((nodes, level) => {
    const boxWidth = Math.min(120, (width - 20) / nodes.length - 8);
    const gap = (width - 20 - boxWidth * nodes.length) / Math.max(1, nodes.length - 1 || 1);
    nodes.forEach((node, i) => {
      const x = nodes.length === 1 ? (width - boxWidth) / 2 : 10 + i * (boxWidth + gap);
      place.set(node.id, { x, y: level * rowHeight + 16, w: boxWidth, h: 34, node });
    });
  });

  const edges = [];
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
function fit(text, budget) {
  if (text.length <= budget) return text;
  const half = Math.max(2, Math.floor((budget - 1) / 2));
  return `${text.slice(0, half)}…${text.slice(-half)}`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

const EXAMPLES = [
  ["a filter and a sort", "SELECT name, city, salary\nFROM people\nWHERE salary > 170000\nORDER BY salary DESC"],
  ["a join", "SELECT p.name, o.product, o.quantity * o.unit_price AS total\nFROM people p JOIN orders o ON p.id = o.person_id\nORDER BY total DESC\nLIMIT 5"],
  ["grouping", "SELECT department, COUNT(*) AS n, AVG(salary) AS avg_salary\nFROM people\nGROUP BY department\nORDER BY n DESC"],
  ["a window function", "SELECT name, department, salary,\n       RANK() OVER (PARTITION BY department ORDER BY salary DESC) AS rank\nFROM people"],
  ["three-valued logic", "SELECT name, score, score > 9 AS high\nFROM people\nWHERE score IS NULL OR score > 9"],
  ["a correlated subquery", "SELECT name FROM people p\nWHERE EXISTS (SELECT 1 FROM orders o WHERE o.person_id = p.id AND o.quantity > 2)"],
  ["set operations", "SELECT city FROM people WHERE salary > 180000\nEXCEPT\nSELECT city FROM people WHERE department = 'Research'"],
  ["an error, with its caret", "SELECT naem FROM people"],
];

let engine;
let storageTable = "people";

async function boot() {
  wasm = await init();
  engine = new QueryEngine();

  const select = $("examples");
  EXAMPLES.forEach(([label, sql], i) => {
    const option = document.createElement("option");
    option.value = String(i);
    option.textContent = label;
    select.appendChild(option);
  });
  select.addEventListener("change", () => {
    $("sql").value = EXAMPLES[Number(select.value)][1];
    run();
  });
  $("sql").value = EXAMPLES[0][1];

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

  $("run").addEventListener("click", run);
  $("sql").addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      run();
    }
  });
  for (const tab of document.querySelectorAll(".tab")) {
    tab.addEventListener("click", () => {
      document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t === tab));
      for (const name of ["results", "plan", "physical", "pipeline", "trace", "storage", "index"]) {
        $(`tab-${name}`).hidden = name !== tab.dataset.tab;
      }
      // Built when opened rather than after every query: neither depends on the
      // last result, and drawing a tree nobody is looking at is waste.
      if (tab.dataset.tab === "storage") renderStorage(storageTable);
      if (tab.dataset.tab === "index") renderIndex();
    });
  }

  $("build").textContent = `wasm module ${(wasmSize / 1024).toFixed(0)} KiB · engine built from scratch, zero dependencies`;
  run();
}

function renderCatalog() {
  const tables = JSON.parse(engine.tables());
  $("catalog").innerHTML = tables
    .map(
      (t) =>
        `<div class="tablecard" data-name="${escapeHtml(t.name)}"><b>${escapeHtml(t.name)}</b> ` +
        `<span>${t.rows.toLocaleString()} rows · ${t.columns.length} cols · ${t.row_groups} row group${t.row_groups === 1 ? "" : "s"}` +
        (t.pending ? " · lazily decoded" : "") +
        `</span><br><span>${t.columns.map((c) => `${escapeHtml(c.name)} ${c.type.toLowerCase()}`).join(", ")}</span></div>`
    )
    .join("");
  for (const card of document.querySelectorAll(".tablecard")) {
    card.addEventListener("click", () => {
      $("sql").value = `SELECT * FROM ${card.dataset.name} LIMIT 20`;
      run();
    });
  }
}

let wasmSize = 0;

function run() {
  const sql = $("sql").value.trim();
  if (!sql) return;
  const error = $("error");
  error.hidden = true;

  let outcome;
  try {
    outcome = engine.query(sql);
  } catch (e) {
    error.hidden = false;
    error.textContent = String(e.message ?? e);
    $("tab-results").textContent = "";
    $("timing").textContent = "";
    return;
  }

  const meta = JSON.parse(outcome.meta);
  renderResults(outcome, meta);
  renderPipeline(meta.stats);
  $("timing").textContent =
    `${meta.num_rows.toLocaleString()} row${meta.num_rows === 1 ? "" : "s"} in ${meta.elapsed_ms.toFixed(3)} ms`;

  try {
    const explain = JSON.parse(engine.explain(sql));
    renderPlan(explain);
    renderTrace(explain);
    renderPhysical(sql);
  } catch {
    // A query can execute and still not re-plan (it cannot, in practice) --
    // but the results are already rendered, so a plan failure must not lose
    // them.
  }
}

fetch("pkg/qe_bg.wasm", { method: "HEAD" })
  .then((r) => { wasmSize = Number(r.headers.get("content-length") ?? 0); })
  .catch(() => {});

boot().catch((e) => {
  document.getElementById("error").hidden = false;
  document.getElementById("error").textContent = `failed to start: ${e.message ?? e}`;
});
