/**
 * The page's handle on the engine.
 *
 * Every method is async because the engine is in a worker now. Two things make
 * that bearable rather than viral:
 *
 * **The catalog is cached.** It changes only when a dataset loads, and half a
 * dozen render paths want it synchronously -- completion, the table switcher,
 * the index picker. So it is fetched once after each load and read from
 * memory afterwards.
 *
 * **Errors keep their shape.** The worker sends the diagnostic the engine
 * rendered, caret and all, and this rethrows it as an `Error` so callers
 * `try`/`catch` exactly as they did when the call was synchronous.
 */
import type { ExecuteDone, Request, Response, ResultChunk } from "./protocol";
import type { CheckInfo, ParseInfo, PhysicalNode, PlanInfo, StorageInfo, TableInfo } from "./types";
import type { IndexInfo, StatsInfo } from "./types";

/** Rows the grid will draw. The engine computes every row either way. */
export const RESULT_CAP = 10_000;

/**
 * Rows a table can hold before a query is run a batch at a time.
 *
 * Below it the message per batch would cost more than the batch. Above it the
 * row counter moving is worth the round trips.
 */
export const STREAM_ABOVE_ROWS = 200_000;

/**
 * `Omit` over a union keeps only the keys every member has, which for a
 * request union is just `kind`. Distributing it over the members first is what
 * makes "a request without its id" mean what it says.
 */
type Unaddressed<T> = T extends unknown ? Omit<T, "id"> : never;

/** Reported while a streamed query runs. */
export interface Progress {
  rows: number;
  chunk: ResultChunk | null;
  stats: StatsInfo | null;
}

export class EngineClient {
  private readonly worker: Worker;
  private next = 1;
  private readonly pending = new Map<
    number,
    { resolve: (v: unknown) => void; reject: (e: Error) => void; onProgress?: (p: Progress) => void }
  >();
  /** The last catalog fetched, so the sync readers below have something to read. */
  private tables: TableInfo[] = [];

  constructor() {
    // `new URL(..., import.meta.url)` is how a bundler is told this is a
    // worker entry point; Vite compiles and hashes it like any other module.
    this.worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
    this.worker.onmessage = (event: MessageEvent<Response>) => this.receive(event.data);
  }

  private receive(response: Response) {
    const waiter = this.pending.get(response.id);
    if (!waiter) return;
    if (response.kind === "progress") {
      waiter.onProgress?.({
        rows: response.rows,
        chunk: response.chunk,
        stats: response.stats as StatsInfo | null,
      });
      return;
    }
    this.pending.delete(response.id);
    if (response.kind === "ok") waiter.resolve(response.value);
    else waiter.reject(new Error(response.message));
  }

  private send<T>(
    request: Unaddressed<Request>,
    onProgress?: (p: Progress) => void,
    transfer: Transferable[] = []
  ): Promise<T> {
    const id = this.next++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, onProgress });
      this.worker.postMessage({ ...request, id } as Request, transfer);
    });
  }

  /** Register a buffer as a table, and refresh the cached catalog. */
  async load(name: string, bytes: Uint8Array): Promise<void> {
    // The buffer is transferred, so the page must not touch it afterwards --
    // and it does not: `fetchDataset` hands its only reference straight here.
    await this.send<string>({ kind: "load", name, bytes }, undefined, [bytes.buffer]);
    await this.refreshCatalog();
  }

  async refreshCatalog(): Promise<TableInfo[]> {
    const json = await this.send<string>({ kind: "catalog" });
    this.tables = JSON.parse(json) as TableInfo[];
    return this.tables;
  }

  /** The catalog as of the last load. Synchronous, which is the whole point. */
  catalog(): TableInfo[] {
    return this.tables;
  }

  async check(sql: string): Promise<CheckInfo | null> {
    const raw = await this.send<string>({ kind: "check", sql });
    return raw === "null" ? null : (JSON.parse(raw) as CheckInfo);
  }

  async parse(sql: string): Promise<ParseInfo> {
    return JSON.parse(await this.send<string>({ kind: "parse", sql })) as ParseInfo;
  }

  async plan(sql: string): Promise<PlanInfo> {
    return JSON.parse(await this.send<string>({ kind: "plan", sql })) as PlanInfo;
  }

  async physicalPlan(sql: string): Promise<PhysicalNode> {
    return JSON.parse(await this.send<string>({ kind: "physicalPlan", sql })) as PhysicalNode;
  }

  async storage(table: string): Promise<StorageInfo> {
    return JSON.parse(await this.send<string>({ kind: "storage", table })) as StorageInfo;
  }

  async indexTree(
    table: string,
    column: string,
    probe: string | undefined,
    perLevel: number
  ): Promise<IndexInfo> {
    const json = await this.send<string>({ kind: "indexTree", table, column, probe, perLevel });
    return JSON.parse(json) as IndexInfo;
  }

  async createIndex(table: string, column: string): Promise<void> {
    await this.send<string>({ kind: "createIndex", table, column });
    await this.refreshCatalog();
  }

  /**
   * Run a query. `onProgress` fires once a frame for a large table, with the
   * rows so far and whatever of them the grid has room for.
   */
  execute(sql: string, onProgress?: (p: Progress) => void): Promise<ExecuteDone> {
    return this.send<ExecuteDone>(
      { kind: "execute", sql, cap: RESULT_CAP, streamAbove: STREAM_ABOVE_ROWS },
      onProgress
    );
  }
}
