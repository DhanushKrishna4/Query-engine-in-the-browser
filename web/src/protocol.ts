/**
 * What crosses the worker boundary.
 *
 * The engine lives in a worker so that a query cannot freeze the tab. That
 * costs one thing and gains another: results have to be *copied* out of wasm
 * memory instead of viewed in place, because a `WebAssembly.Memory` buffer is
 * not transferable and never will be -- but the copy is bounded by what the
 * grid draws, and the main thread stays free to paint while a query runs.
 *
 * Every reply is either `ok` with a value or `error` with a rendered
 * diagnostic, caret and all, so the page shows the same error the terminal
 * would.
 */
import type { GroupVerdict, OutcomeMeta } from "./types";

/** A result column, copied out of wasm memory into buffers we can transfer. */
export interface ColumnData {
  kind: number;
  rows: number;
  /** One bit per row, or null when the column has no NULLs at all. */
  validity: Uint8Array | null;
  /** The dense typed buffer. `Utf8` puts its bytes here and its ends in `offsets`. */
  values: ArrayBuffer;
  offsets: Uint32Array | null;
}

/** One result, ready to draw: the metadata and as many rows as were asked for. */
export interface ResultChunk {
  meta: OutcomeMeta;
  columns: ColumnData[];
  /** Rows in the chunk, which is not `columns[0].rows` once a cap has bitten. */
  rows: number;
}

export type Request =
  | { id: number; kind: "load"; name: string; bytes: Uint8Array }
  | { id: number; kind: "catalog" }
  | { id: number; kind: "check"; sql: string }
  | { id: number; kind: "parse"; sql: string }
  | { id: number; kind: "plan"; sql: string }
  | { id: number; kind: "physicalPlan"; sql: string }
  | { id: number; kind: "storage"; table: string }
  | { id: number; kind: "indexTree"; table: string; column: string; probe?: string; perLevel: number }
  | { id: number; kind: "createIndex"; table: string; column: string }
  /**
   * Run a query. `cap` bounds the rows copied back; `streamAbove` is the table
   * size past which the worker reports progress as it goes rather than
   * answering once at the end.
   */
  | { id: number; kind: "execute"; sql: string; cap: number; streamAbove: number };

export type Response =
  | { id: number; kind: "ok"; value: unknown }
  | { id: number; kind: "error"; message: string }
  /** Sent zero or more times before the `ok` that ends an `execute`. */
  | { id: number; kind: "progress"; rows: number; chunk: ResultChunk | null; stats: unknown };

/** The `ok` value of an `execute`, once the last chunk has been sent. */
export interface ExecuteDone {
  meta: OutcomeMeta;
  rows: number;
  /** True when chunks were streamed, so the page knows it has already drawn. */
  streamed: boolean;
  chunk: ResultChunk | null;
}

/** Re-exported so the worker and the page agree on one definition. */
export type { GroupVerdict };
