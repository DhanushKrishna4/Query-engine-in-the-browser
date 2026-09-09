// This engine against sql.js, in one tab, on one clock.
//
// Both get identical rows and identical SQL. Both are timed with
// `performance.now()` and reported as the minimum of N runs, which is the same
// methodology the native benchmarks use: the minimum is the run least disturbed
// by everything else the machine was doing.
//
// Every query's results are compared between the two engines before its timing
// is believed. A benchmark that is faster because it computed something else is
// not a benchmark, and running two independent SQL implementations over the
// same data makes that check nearly free.
import init, { QueryEngine, ColumnKind } from "./pkg/qe.js";

const $ = (id) => document.getElementById(id);
const decoder = new TextDecoder();

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/** Seeded so a rerun measures the same rows, not similar ones. */
function mulberry32(seed) {
  return function () {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * A standard normal, by Box-Muller.
 *
 * Needed because trip distances are lognormal: most short, a few very long.
 * Scaling a uniform instead gives a distribution with no tail at all, and a
 * benchmark query like `distance > 25` then matches nothing and measures two
 * engines agreeing that the answer is empty.
 */
function gaussian(random) {
  let u = 0;
  while (u === 0) u = random();
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * random());
}

const VENDORS = ["yellow", "green", "fhv"];
const BOROUGHS = ["Manhattan", "Brooklyn", "Queens", "Bronx", "Staten Island"];

/** Taxi-shaped rows, as a CSV string and as an array for the INSERT path. */
function generate(count) {
  const random = mulberry32(7);
  const rows = new Array(count);
  const parts = ["id,vendor,borough,passengers,distance,fare,tip,day\n"];
  for (let i = 0; i < count; i++) {
    const distance = Math.round(Math.exp(0.6 + 0.8 * gaussian(random)) * 100) / 100;
    const fare = Math.round((2.5 + distance * 2.75) * 100) / 100;
    // About one tip in fifty is missing, so both engines meet NULLs.
    const tip =
      random() < 0.02 ? null : Math.round(fare * [0, 0.1, 0.15, 0.2, 0.25][(random() * 5) | 0] * 100) / 100;
    const vendor = VENDORS[(random() * 3) | 0];
    const borough = BOROUGHS[(random() * 5) | 0];
    const passengers = 1 + ((random() * 6) | 0);
    const day = 1 + ((random() * 365) | 0);
    rows[i] = [i, vendor, borough, passengers, distance, fare, tip, day];
    parts.push(
      i + "," + vendor + "," + borough + "," + passengers + "," + distance + "," + fare + "," +
        (tip === null ? "" : tip) + "," + day + "\n"
    );
  }
  return { rows, csv: parts.join("") };
}

const BOROUGH_ROWS = [
  ["Manhattan", "core", 2.75],
  ["Brooklyn", "outer", 1.5],
  ["Queens", "outer", 1.5],
  ["Bronx", "outer", 1.0],
  ["Staten Island", "outer", 0.0],
];
const BOROUGH_CSV = "borough,region,fee\n" + BOROUGH_ROWS.map((r) => r.join(",")).join("\n") + "\n";

// ---------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------

const QUERIES = [
  ["count every row", "SELECT COUNT(*) FROM trips"],
  ["four aggregates, no grouping", "SELECT COUNT(*), AVG(fare), MIN(tip), MAX(distance) FROM trips"],
  ["filtered count", "SELECT COUNT(*) FROM trips WHERE fare > 45"],
  ["group by 3 values", "SELECT vendor, COUNT(*), AVG(fare) FROM trips GROUP BY vendor"],
  ["group by 15 pairs", "SELECT borough, vendor, COUNT(*), SUM(fare) FROM trips GROUP BY borough, vendor"],
  ["group by 365 days", "SELECT day, COUNT(*), AVG(fare) FROM trips GROUP BY day"],
  ["string equality", "SELECT COUNT(*) FROM trips WHERE borough = 'Bronx'"],
  ["NULL test", "SELECT COUNT(*) FROM trips WHERE tip IS NULL"],
  ["arithmetic in the predicate", "SELECT COUNT(*) FROM trips WHERE fare - tip > 40"],
  // Selective but not empty: the lognormal tail puts roughly one trip in
  // three hundred past twelve miles.
  ["selective, returns rows", "SELECT id, fare FROM trips WHERE distance > 12"],
  // `id` breaks the tie. Without it the ten highest fares are not a unique
  // answer -- fares repeat, so each engine may pick different rows among the
  // equals and both are right. Comparing results demands the query pin the
  // order it wants.
  ["top ten", "SELECT id, fare FROM trips ORDER BY fare DESC, id LIMIT 10"],
  ["distinct pairs", "SELECT DISTINCT borough, vendor FROM trips"],
  [
    "join to a dimension",
    "SELECT b.region, COUNT(*), AVG(t.fare) FROM trips t JOIN boroughs b ON t.borough = b.borough GROUP BY b.region",
  ],
  ["point lookup by id", "SELECT id, fare FROM trips WHERE id = 123456"],
  ["a narrow range", "SELECT COUNT(*) FROM trips WHERE id BETWEEN 100000 AND 100100"],
];

/** Queries whose row order is not pinned by the SQL, so compare as multisets. */
const UNORDERED = new Set(QUERIES.map((q) => q[1]).filter((s) => !/ORDER BY/.test(s)));

// ---------------------------------------------------------------------------
// Running each engine
// ---------------------------------------------------------------------------

let wasm;
let engine;
let db;

/** Read a result out of wasm memory as plain JS values. */
function readOurs(outcome, meta) {
  const buffer = wasm.memory.buffer;
  const views = meta.columns.map((_, i) => {
    const kind = outcome.kind(i);
    const rows = outcome.len(i);
    const ptr = outcome.valuesPtr(i);
    const vp = outcome.validityPtr(i);
    const validity = vp === 0 ? null : new Uint8Array(buffer, vp, Math.ceil(rows / 8));
    switch (kind) {
      case ColumnKind.Int32:
      case ColumnKind.Date32:
        return { kind, validity, values: new Int32Array(buffer, ptr, rows) };
      case ColumnKind.Int64:
      case ColumnKind.Timestamp:
        return { kind, validity, values: new BigInt64Array(buffer, ptr, rows) };
      case ColumnKind.Float64:
        return { kind, validity, values: new Float64Array(buffer, ptr, rows) };
      case ColumnKind.Boolean:
        return { kind, validity, values: new Uint8Array(buffer, ptr, Math.ceil(rows / 8)) };
      case ColumnKind.Utf8:
        return {
          kind,
          validity,
          values: new Uint8Array(buffer, ptr, outcome.valuesBytes(i)),
          offsets: new Uint32Array(buffer, outcome.offsetsPtr(i), rows + 1),
        };
      default:
        return { kind, validity, values: null };
    }
  });

  const out = [];
  const rows = views[0] ? outcome.len(0) : 0;
  for (let r = 0; r < rows; r++) {
    const row = [];
    for (const v of views) {
      if (v.validity && (v.validity[r >> 3] & (1 << (r & 7))) === 0) {
        row.push(null);
      } else if (v.kind === ColumnKind.Utf8) {
        row.push(decoder.decode(v.values.subarray(v.offsets[r], v.offsets[r + 1])));
      } else if (v.kind === ColumnKind.Boolean) {
        row.push((v.values[r >> 3] & (1 << (r & 7))) !== 0);
      } else if (v.kind === ColumnKind.Int64) {
        row.push(Number(v.values[r]));
      } else {
        row.push(v.values[r]);
      }
    }
    out.push(row);
  }
  return out;
}

function runOurs(sql) {
  const outcome = engine.query(sql);
  return readOurs(outcome, JSON.parse(outcome.meta));
}

function runSqlite(sql) {
  const result = db.exec(sql);
  return result.length === 0 ? [] : result[0].values;
}

/**
 * Normalize a result so two engines can be compared.
 *
 * Floats are rounded: an AVG summed in a different order differs in the last
 * bit or two, and that is not a disagreement worth failing on. Unordered
 * results are sorted, because neither engine promises an order the SQL did not
 * ask for.
 */
function normalize(rows, unordered) {
  const text = rows.map((row) =>
    row
      .map((v) => {
        if (v === null) return "NULL";
        if (typeof v === "number") return Number.isInteger(v) ? String(v) : v.toPrecision(10);
        if (typeof v === "bigint") return String(v);
        return String(v);
      })
      .join("")
  );
  if (unordered) text.sort();
  return text;
}

/**
 * The finest interval `performance.now()` distinguishes here.
 *
 * Browsers coarsen it deliberately. Anything below this is not a small
 * measurement, it is no measurement, and dividing two of them produces a ratio
 * with no meaning -- which is how an indexed lookup first came out as `0.00x`.
 */
const CLOCK_RESOLUTION_MS = 0.1;

/** Minimum of `runs`, after one untimed warm-up. */
function time(fn, runs) {
  fn();
  let best = Infinity;
  for (let i = 0; i < runs; i++) {
    const start = performance.now();
    fn();
    best = Math.min(best, performance.now() - start);
  }
  return best;
}

// ---------------------------------------------------------------------------
// Driving it
// ---------------------------------------------------------------------------

const say = (html) => {
  $("status").innerHTML = html;
};
const yieldToPaint = () => new Promise((r) => setTimeout(r, 0));

async function boot() {
  wasm = await init();
  const SQL = await initSqlJs({ locateFile: (f) => "vendor/" + f });
  say('<p class="empty">both engines loaded. Pick a size and run.</p>');
  $("go").addEventListener("click", () =>
    run(SQL).catch((e) => {
      say('<div class="error">' + esc(e.message ?? e) + "</div>");
      $("go").disabled = false;
    })
  );
}

async function run(SQL) {
  const count = Number($("rows").value);
  const runs = Number($("runs").value);
  $("go").disabled = true;
  $("results-panel").hidden = true;
  $("notes-panel").hidden = true;

  say('<p class="empty">generating ' + count.toLocaleString() + " rows…</p>");
  await yieldToPaint();
  const data = generate(count);

  say('<p class="empty">loading ' + count.toLocaleString() + " rows into both engines…</p>");
  await yieldToPaint();

  engine = new QueryEngine();
  const bytes = new TextEncoder().encode(data.csv);
  const ourLoadStart = performance.now();
  engine.load("trips", bytes);
  engine.load("boroughs", new TextEncoder().encode(BOROUGH_CSV));
  const ourLoad = performance.now() - ourLoadStart;

  if (db) db.close();
  db = new SQL.Database();
  const sqliteLoadStart = performance.now();
  db.run(
    "CREATE TABLE trips (id INTEGER, vendor TEXT, borough TEXT, passengers INTEGER," +
      " distance REAL, fare REAL, tip REAL, day INTEGER);" +
      "CREATE TABLE boroughs (borough TEXT, region TEXT, fee REAL);"
  );
  db.run("BEGIN");
  const insert = db.prepare("INSERT INTO trips VALUES (?,?,?,?,?,?,?,?)");
  for (const row of data.rows) insert.run(row);
  insert.free();
  const insertBorough = db.prepare("INSERT INTO boroughs VALUES (?,?,?)");
  for (const row of BOROUGH_ROWS) insertBorough.run(row);
  insertBorough.free();
  db.run("COMMIT");
  const sqliteLoad = performance.now() - sqliteLoadStart;

  const results = [];
  for (const q of QUERIES) {
    say('<p class="empty">running: ' + esc(q[0]) + "…</p>");
    await yieldToPaint();
    results.push(measure(q[0], q[1], runs));
  }

  // SQLite with an index is its home ground, and the point of running this at
  // all is to see where each architecture actually lives.
  say('<p class="empty">giving both engines an index on id and rerunning the lookups…</p>');
  await yieldToPaint();
  const indexStart = performance.now();
  db.run("CREATE INDEX trips_id ON trips(id)");
  const sqliteIndex = performance.now() - indexStart;
  const ourIndexStart = performance.now();
  engine.createIndex("trips", "id");
  const ourIndex = performance.now() - ourIndexStart;

  const indexed = QUERIES.slice(-2).map((q) => measure(q[0], q[1], runs));

  render({ count, runs, results, indexed, ourLoad, sqliteLoad, sqliteIndex, ourIndex });
  $("go").disabled = false;
}

function measure(label, sql, runs) {
  try {
    const a = normalize(runOurs(sql), UNORDERED.has(sql));
    const b = normalize(runSqlite(sql), UNORDERED.has(sql));
    const agreed = a.length === b.length && a.every((x, i) => x === b[i]);
    return {
      label,
      sql,
      rows: a.length,
      agreed: agreed ? "yes" : "NO",
      ours: time(() => runOurs(sql), runs),
      theirs: time(() => runSqlite(sql), runs),
    };
  } catch (e) {
    return { label, sql, rows: 0, agreed: "error: " + (e.message ?? e), ours: null, theirs: null };
  }
}

function render(r) {
  say(
    '<p class="empty">' +
      r.count.toLocaleString() +
      " rows · minimum of " +
      r.runs +
      " runs after a warm-up · every result checked against the other engine's before it was timed</p>"
  );

  const rowHtml = (x) => {
    if (x.ours === null) {
      return "<tr><td>" + esc(x.label) + '</td><td colspan="5" class="null">' + esc(x.agreed) + "</td></tr>";
    }
    const measurable = x.ours >= CLOCK_RESOLUTION_MS && x.theirs >= CLOCK_RESOLUTION_MS;
    const ratio = x.theirs / x.ours;
    const colour = ratio >= 1 ? "var(--accent)" : "var(--warn)";
    const verdict = measurable
      ? '<td class="num" style="color:' + colour + '">' + ratio.toFixed(2) + "x</td>"
      : '<td class="num null" title="below what performance.now() can measure">too fast</td>';
    return (
      '<tr><td title="' + esc(x.sql) + '">' + esc(x.label) + "</td>" +
      '<td class="num">' + (x.ours < CLOCK_RESOLUTION_MS ? "&lt;0.1" : x.ours.toFixed(2)) + "</td>" +
      '<td class="num">' + (x.theirs < CLOCK_RESOLUTION_MS ? "&lt;0.1" : x.theirs.toFixed(2)) + "</td>" +
      verdict +
      '<td class="num">' + x.rows.toLocaleString() + "</td>" +
      '<td class="' + (x.agreed === "yes" ? "" : "null") + '">' + esc(x.agreed) + "</td></tr>"
    );
  };

  $("table").innerHTML =
    '<table><thead><tr><th>query</th><th class="num">this engine</th><th class="num">sql.js</th>' +
    '<th class="num">ratio</th><th class="num">rows</th><th>agreed</th></tr></thead><tbody>' +
    r.results.map(rowHtml).join("") +
    '<tr><td colspan="6" style="padding-top:.8rem;color:var(--dim)">— with an index on <code>id</code> in both engines —</td></tr>' +
    r.indexed.map(rowHtml).join("") +
    "</tbody></table>";
  $("method").textContent = r.count.toLocaleString() + " rows, min of " + r.runs;
  $("results-panel").hidden = false;

  const timed = r.results.filter(
    (x) => x.ours !== null && x.ours >= CLOCK_RESOLUTION_MS && x.theirs >= CLOCK_RESOLUTION_MS
  );
  const wins = timed.filter((x) => x.theirs / x.ours >= 1).length;
  const disagreed = r.results.concat(r.indexed).filter((x) => x.agreed !== "yes");

  $("notes").innerHTML =
    "<p>A ratio above 1.00x means this engine was faster by that much.</p>" +
    "<p>Loading: <b>" + r.ourLoad.toFixed(0) + " ms</b> here against <b>" + r.sqliteLoad.toFixed(0) +
    " ms</b> for sql.js. Not like for like — this engine parses CSV, sql.js runs " +
    r.count.toLocaleString() + " prepared <code>INSERT</code>s inside one transaction — but it is the cost " +
    "each actually pays to start answering questions about a file. The index on <code>id</code> took " +
    r.sqliteIndex.toFixed(0) + " ms in SQLite and " + r.ourIndex.toFixed(0) + " ms here.</p>" +
    "<p>Of the " + timed.length + " queries both engines took long enough to measure, " + wins +
    " went this way and " + (timed.length - wins) +
    " to sql.js. The split is the one the architectures predict: wide scans and grouping favour a columnar, " +
    "vectorized engine that reads the two columns a query names out of eight and compares 2048 values at a " +
    "time. Point lookups favour a B-tree, and once SQLite has an index on <code>id</code> it answers them " +
    "without touching the table at all.</p>" +
    (disagreed.length === 0
      ? '<p style="color:var(--accent)">Every query returned identical results from both engines.</p>'
      : '<p style="color:var(--error)">' + disagreed.length + " query result(s) differed: " +
        disagreed.map((d) => esc(d.label)).join(", ") +
        ". A timing means nothing without this line saying no.</p>");
  $("notes-panel").hidden = false;
}

function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

boot().catch((e) => say('<div class="error">failed to start: ' + esc(e.message ?? e) + "</div>"));
