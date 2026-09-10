/**
 * The shapes the wasm boundary hands back as JSON.
 *
 * Written out rather than inferred because they are a contract with
 * `crates/wasm/src/lib.rs`: every field here is a `#[derive(Serialize)]`
 * struct on the other side, and TypeScript is the only thing that will notice
 * when the two drift apart.
 */

export interface FieldInfo {
  name: string;
  type: string;
  nullable: boolean;
  kind: number;
}

export interface StatsInfo {
  name: string;
  detail: string;
  reason: string | null;
  rows_in: number;
  rows_out: number;
  batches: number;
  elapsed_ms: number;
  peak_batch_bytes: number;
  estimated_rows: number | null;
  q_error: number | null;
  row_groups_total: number;
  row_groups_scanned: number;
  row_groups_pruned: number;
  row_groups_bloom_pruned: number;
  /** Scans only: the table read, and what became of each of its row groups. */
  scanned_table: string | null;
  row_group_verdicts: GroupVerdict[];
  compactions: number;
  children: StatsInfo[];
}

/** What a scan did with one row group, in the order they appear in the file. */
export type GroupVerdict = "untouched" | "scanned" | "zone map" | "bloom" | "encoding";

export interface OutcomeMeta {
  columns: FieldInfo[];
  num_rows: number;
  elapsed_ms: number;
  truncated: boolean;
  stats: StatsInfo;
}

export interface TableInfo {
  name: string;
  rows: number;
  columns: FieldInfo[];
  row_groups: number;
  bytes: number;
  pending: boolean;
  indexes: string[];
}

export interface PlanNode {
  label: string;
  rel: number;
  children: PlanNode[];
}

export interface TraceStep {
  rule: string;
  target: number;
  before: PlanNode;
  after: PlanNode;
}

/** One token, and the byte range of the source it was read from. */
export interface TokenInfo {
  kind: string;
  text: string;
  start: number;
  end: number;
}

/** What `parse` returns: the two stages before any name is resolved. */
export interface ParseInfo {
  tokens: TokenInfo[];
  /** `ast::pretty` output: two spaces per level, one node per line. */
  ast: string;
}

/** What `plan` returns: everything from binding onwards. */
export interface PlanInfo {
  bound: string;
  optimized: string;
  typed: string;
  steps: TraceStep[];
  truncated: boolean;
}

export interface PhysicalNode {
  name: string;
  detail: string;
  reason: string | null;
  estimated_rows: number | null;
  children: PhysicalNode[];
}

export interface ZoneInfo {
  name: string;
  type: string;
  min: string | null;
  max: string | null;
  /** null when the file recorded no null count -- unknown, not zero. */
  nulls: number | null;
  distinct: number | null;
  bloom: boolean;
  encoding: string | null;
  ratio: number | null;
}

export interface RowGroupInfo {
  rows: number;
  bytes: number;
  resident: number | null;
  columns: ZoneInfo[];
}

export interface StorageInfo {
  table: string;
  rows: number;
  bytes: number;
  pending: boolean;
  row_groups: RowGroupInfo[];
}

export interface TreeNodeInfo {
  id: number;
  level: number;
  keys: number;
  first: string | null;
  last: string | null;
  leaf: boolean;
  children: number[];
  level_total: number;
  on_path: boolean;
}

export interface IndexInfo {
  table: string;
  column: string;
  height: number;
  keys: number;
  entries: number;
  nodes: number;
  leaves: number;
  bytes: number;
  level_widths: number[];
  tree: TreeNodeInfo[];
  path: number[];
  matched: number | null;
}

export interface CheckInfo {
  message: string;
  hint: string | null;
  stage: string;
  start: number;
  end: number;
}
