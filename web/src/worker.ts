/**
 * The engine, off the main thread.
 *
 * Everything below the boundary is Rust; this file is the only place that
 * touches it now. The page talks to it through `client.ts` and never sees a
 * `QueryEngine` at all.
 *
 * ## The copy, and why it is the right trade
 *
 * On the main thread a result could be *viewed* in place: typed arrays built
 * straight onto the module's memory, no copy at any size. A worker cannot do
 * that, because a `WebAssembly.Memory` buffer is not transferable -- so each
 * column is copied into a fresh buffer and transferred.
 *
 * The copy is bounded by what the grid actually draws (10,000 rows), not by
 * what the query computed, so a seven-million-row aggregate scan still crosses
 * as a few hundred kilobytes. In exchange the tab never freezes: the whole
 * query, not just the gaps between batches, runs somewhere the renderer is not.
 */
import init, { ColumnKind, QueryEngine } from "../pkg/qe.js";
import type { ColumnData, ExecuteDone, Request, Response, ResultChunk } from "./protocol";
import type { OutcomeMeta } from "./types";

/** The wasm module's exports, for reading typed arrays out of its memory. */
let wasm: { memory: WebAssembly.Memory };
let engine: QueryEngine;

type Outcome = ReturnType<QueryEngine["execute"]>;

const ready = (async () => {
  wasm = await init();
  engine = new QueryEngine();
})();

/**
 * Copy one column out of wasm memory, up to `cap` rows.
 *
 * The views are built and consumed inside this function on purpose: a view
 * onto wasm memory is valid only until wasm allocates again, and `slice`
 * copies immediately.
 */
function copyColumn(outcome: Outcome, index: number, cap: number): ColumnData {
  const buffer = wasm.memory.buffer;
  const kind = outcome.kind(index);
  const total = outcome.len(index);
  const rows = Math.min(total, cap);
  const ptr = outcome.valuesPtr(index);
  const validityPtr = outcome.validityPtr(index);
  // A null pointer means no row in the column is NULL, so the page can skip
  // the per-row check entirely.
  const validity =
    validityPtr === 0
      ? null
      : new Uint8Array(buffer, validityPtr, Math.ceil(total / 8)).slice(
          0,
          Math.ceil(rows / 8)
        );

  const empty = (): ColumnData => ({ kind, rows, validity, values: new ArrayBuffer(0), offsets: null });

  switch (kind) {
    case ColumnKind.Int32:
    case ColumnKind.Date32:
      return { kind, rows, validity, values: new Int32Array(buffer, ptr, rows).slice().buffer, offsets: null };
    case ColumnKind.Int64:
    case ColumnKind.Timestamp:
      return { kind, rows, validity, values: new BigInt64Array(buffer, ptr, rows).slice().buffer, offsets: null };
    case ColumnKind.Float64:
      return { kind, rows, validity, values: new Float64Array(buffer, ptr, rows).slice().buffer, offsets: null };
    case ColumnKind.Boolean:
      return {
        kind,
        rows,
        validity,
        values: new Uint8Array(buffer, ptr, Math.ceil(rows / 8)).slice().buffer,
        offsets: null,
      };
    case ColumnKind.Utf8: {
      // Only the bytes the kept rows point at, so a capped result does not
      // copy the whole string buffer behind it.
      const allOffsets = new Uint32Array(buffer, outcome.offsetsPtr(index), total + 1);
      const offsets = allOffsets.slice(0, rows + 1);
      const bytes = rows === 0 ? 0 : offsets[rows];
      return {
        kind,
        rows,
        validity,
        values: new Uint8Array(buffer, ptr, bytes).slice().buffer,
        offsets,
      };
    }
    default:
      return empty();
  }
}

function copyChunk(outcome: Outcome, cap: number): ResultChunk {
  const meta = JSON.parse(outcome.meta) as OutcomeMeta;
  const columns = meta.columns.map((_, i) => copyColumn(outcome, i, cap));
  return { meta, columns, rows: columns[0]?.rows ?? 0 };
}

/** Everything transferable in a chunk, so the copy crosses without a second one. */
function transfers(chunk: ResultChunk | null): Transferable[] {
  if (!chunk) return [];
  const out: Transferable[] = [];
  for (const c of chunk.columns) {
    out.push(c.values);
    if (c.validity) out.push(c.validity.buffer);
    if (c.offsets) out.push(c.offsets.buffer);
  }
  return out;
}

const reply = (r: Response, transfer: Transferable[] = []) =>
  (self as unknown as Worker).postMessage(r, transfer);

/**
 * Run a query, streaming when the data is big enough to be worth it.
 *
 * The threshold lives here rather than on the page because the worker is what
 * knows how long the query has been running -- and because below it the
 * message per batch would cost more than the batch.
 */
function execute(id: number, sql: string, cap: number, streamAbove: number) {
  const biggest = (JSON.parse(engine.catalog()) as { rows: number }[]).reduce(
    (n, t) => Math.max(n, t.rows),
    0
  );

  if (biggest < streamAbove) {
    const outcome = engine.execute(sql);
    const chunk = copyChunk(outcome, cap);
    const done: ExecuteDone = { meta: chunk.meta, rows: chunk.meta.num_rows, streamed: false, chunk };
    reply({ id, kind: "ok", value: done }, transfers(chunk));
    return;
  }

  const progress = engine.executeStreaming(sql);
  let shown = 0;
  let total = 0;
  let schema: OutcomeMeta | null = null;
  // A message per batch would be three thousand messages on a seven-million-row
  // scan for sixty useful repaints. One per frame is what the page can draw.
  const FRAME_MS = 16;
  let frameStart = performance.now();

  for (;;) {
    const batch = progress.next();
    if (!batch) break;
    const remaining = Math.max(0, cap - shown);
    const chunk = remaining > 0 ? copyChunk(batch, remaining) : null;
    if (!schema) schema = (chunk?.meta ?? (JSON.parse(batch.meta) as OutcomeMeta)) as OutcomeMeta;
    shown += chunk?.rows ?? 0;
    total += batch.len(0);

    if (chunk || performance.now() - frameStart >= FRAME_MS) {
      // The statistics tree is serialized afresh on every read, so it goes
      // with a frame rather than with a batch.
      const stats = performance.now() - frameStart >= FRAME_MS ? JSON.parse(progress.stats) : null;
      reply({ id, kind: "progress", rows: total, chunk, stats }, transfers(chunk));
      frameStart = performance.now();
    }
  }

  const summary = JSON.parse(progress.progress) as { rows: number; elapsed_ms: number };
  const meta = {
    ...(schema as OutcomeMeta),
    stats: JSON.parse(progress.stats),
    num_rows: summary.rows,
    elapsed_ms: summary.elapsed_ms,
  };
  const done: ExecuteDone = { meta, rows: summary.rows, streamed: true, chunk: null };
  reply({ id, kind: "ok", value: done });
}

self.onmessage = async (event: MessageEvent<Request>) => {
  await ready;
  const request = event.data;
  const { id } = request;
  try {
    switch (request.kind) {
      case "load":
        reply({ id, kind: "ok", value: engine.load(request.name, request.bytes) });
        break;
      case "catalog":
        reply({ id, kind: "ok", value: engine.catalog() });
        break;
      case "check":
        reply({ id, kind: "ok", value: engine.check(request.sql) });
        break;
      case "parse":
        reply({ id, kind: "ok", value: engine.parse(request.sql) });
        break;
      case "plan":
        reply({ id, kind: "ok", value: engine.plan(request.sql) });
        break;
      case "physicalPlan":
        reply({ id, kind: "ok", value: engine.physicalPlan(request.sql) });
        break;
      case "storage":
        reply({ id, kind: "ok", value: engine.storage(request.table) });
        break;
      case "indexTree":
        reply({
          id,
          kind: "ok",
          value: engine.indexTree(request.table, request.column, request.probe, request.perLevel),
        });
        break;
      case "createIndex":
        reply({ id, kind: "ok", value: engine.createIndex(request.table, request.column) });
        break;
      case "execute":
        execute(id, request.sql, request.cap, request.streamAbove);
        break;
    }
  } catch (e) {
    // Diagnostics arrive from the engine already rendered, caret and all.
    reply({ id, kind: "error", message: e instanceof Error ? e.message : String(e) });
  }
};
