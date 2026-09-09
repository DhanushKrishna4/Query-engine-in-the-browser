# Query engine in the browser

### **→ [dhanushkrishna4.github.io/Query-engine-in-the-browser](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/)**

An analytical SQL query engine written from scratch in Rust, compiled to
WebAssembly and running entirely in a browser tab. There is no backend: the
lexer, the parser, the binder, the optimizer and every operator execute in the
page. Open the link, type a query, and watch each stage of the pipeline -- the
plan, every optimizer rewrite, the row groups it skipped, the B+ tree it
descended, and what each operator actually cost.

No SQL parser crates, no DataFusion/Polars/DuckDB, no Arrow. The engine crate
has **zero dependencies** and CI fails if it grows one.

**Status: the twenty build-order steps are done**, with real exceptions rather
than a clean sweep -- the storage layer has no compression encodings of its own
and several rewrite rules the spec lists are missing. Those and twenty others are written down in
[Deliberate gaps](#deliberate-gaps); nothing is claimed there that is not
true here.

Headline ratios, every one measured in a single sitting against the same engine
with one thing turned off -- so they are comparable to each other, which they
were not when each was taken on the day its feature landed:

| turning off | costs | on |
| --- | ---: | --- |
| vectorized evaluation | **4.8x** | ten filters and projections over a million rows |
| predicate pushdown | **132.7x** | a join whose filter sat above it |
| the semi-join rewrite | **113.6x** | `IN (SELECT ...)` returning ~26,000 values |
| zone maps | **9.5x** | a clustered predicate matching 1% of rows |
| the B+ tree index | **13.7x** | a predicate on an unclustered column matching 963 rows in a million |
| the top-N heap | **6.2x** | two sort keys with `LIMIT 20 OFFSET 100` |
| cost-based join ordering | **1.7x** | a join written in the wrong order |
| bloom filters | **349x** | an absent value inside every row group's range |

And against [sql.js](#against-sqljs) — SQLite compiled to WebAssembly — over
200,000 rows in the same tab: 12 of 14 measurable queries went this way, up to
36x on filtered aggregates. The two it lost are in that section, with why.

Every operator reports what the optimizer predicted beside what it actually
produced, because the gap between them is the most informative number a query
optimizer has and almost nothing shows it to you.

```
crates/engine     the whole pipeline. no dependencies at all, builds for wasm32.
crates/cli        native REPL -- the primary development surface.
crates/wasm       the wasm boundary. the only crate that knows JavaScript exists.
web/              the page: editor, plan, pipeline, storage, index, benchmark.
data/             small sample tables.
benches/          benchmark query sets.
tests/sqllogictest/   the differential corpus.
tools/            dataset prep, expected-output generation, query fuzzing, the web build.
```

## Try it

```bash
cargo run --bin qe -- --load people=data/people.csv
```

```
qe> SELECT name, city FROM people WHERE age > 40 AND city = 'London';
name            | city
----------------+-------
Alan Turing     | London
Tim Berners-Lee | London
2 rows in 0.219 ms
```

Statements end at `;`; until then input accumulates across lines. `.help` lists
the commands. The interesting ones mirror the tabs the browser UI will have:

| command | shows |
| --- | --- |
| `.tokens <sql>` | lexer output with byte offsets |
| `.ast <sql>` | the parse tree |
| `.bound <sql>` | the logical plan as the binder produced it |
| `.plan <sql>` | the optimized plan, with resolved types on every expression |
| `.trace <sql>` | every optimizer rewrite, one complete plan per step |
| `.rowgroups <t>` | row groups, their zone maps, bloom filters and (for Parquet) which columns have been decoded |
| `.index <t> <c>` | build a B+ tree index on a column |
| `.indexes [t]` | indexes, their height and the width of each level |
| `.stats on` | per-operator rows, time and row groups touched after each query |
| `.schema [t]`, `.tables`, `.load <name> <file>` | catalog |

One-shot: `qe --load people=data/people.csv -c "SELECT ..."`. `--load` takes
CSV or Parquet and decides by the file's first four bytes.
Run one test file: `qe --slt tests/sqllogictest/null.slt`.
Benchmark: `qe --load trips=/tmp/trips.csv --bench benches/taxi.sql`.

In a browser, without installing anything:
**[the live site](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/)**,
and **[/bench.html](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/bench.html)**
for the head-to-head against sql.js. To run that same page locally against your
own build, `tools/build_web.sh --serve` serves it on http://localhost:8137.

For volume, `tools/gen_sample.py 1000000 > /tmp/trips.csv` writes a synthetic
NYC-taxi-shaped CSV. Real datasets are never committed -- in the browser they
come from a CDN and are cached in IndexedDB.

### About the numbers in this file

Every benchmark table below was measured in a single sitting: one machine, one
release build, the same 1M-row 9-column fixture, minimum of 9 runs per query.
They were previously taken step by step over many sessions, which made absolute
times incomparable across sections -- one table could be 2x slower than another
purely because of what else the machine was doing that day.

Each table is a same-process ratio: the same engine and the same data with one
thing turned off, so **the speedup column is the number to read**. Absolute
times still drift with machine state, and a couple of ratios legitimately moved
when later build steps removed the work an earlier step was being credited for
-- both places where that happened say so.

## What works

**SQL.**

```sql
SELECT [DISTINCT] [items] FROM table [alias]
  [{INNER|LEFT|RIGHT|FULL|CROSS} JOIN table [alias] ON pred]...
  [WHERE pred] [GROUP BY exprs] [HAVING pred]
  [{UNION|INTERSECT|EXCEPT} [ALL] SELECT ...]...
  [ORDER BY exprs [ASC|DESC] [NULLS FIRST|LAST]] [LIMIT n [OFFSET m]]

WITH name [(cols)] AS (query) [, ...] SELECT ...
```

with derived tables (`FROM (SELECT ...) x`) anywhere a table can go, over a
complete expression grammar: arithmetic, comparison, `AND`/`OR`/`NOT`, `||`,
`IS [NOT] NULL`, `BETWEEN`, `IN (list)`, `LIKE`/`ILIKE` with `ESCAPE`, `CASE`
(simple and searched), `CAST`, qualified and quoted identifiers, and subqueries
-- scalar, `[NOT] EXISTS` and `[NOT] IN`, correlated or not. Comma joins are
cross joins. Aggregates are `COUNT(*)`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`,
each with an optional `DISTINCT`, and window functions -- `ROW_NUMBER`, `RANK`,
`DENSE_RANK`, `LAG`, `LEAD` and the aggregates -- over
`OVER (PARTITION BY ... ORDER BY ... [ROWS|RANGE frame])`. Common table expressions bind as named
relations and shadow a catalog table of the same name; a CTE referenced twice is
materialized once and both references read the same buffer. Anything outside
that subset -- `RECURSIVE` CTEs, `USING`, `GROUPS` frames, `RANGE` frames with
numeric offsets -- is recognised and rejected by name, so you get
`common table expressions is not supported yet` rather than a confusing parse
error.

**Types.** Boolean, Int32, Int64, Float64, Utf8, Date32, Timestamp, Decimal128.
Implicit widening (Int32→Int64→Float64, Date32→Timestamp) and string-literal
folding (`WHERE hired > '2002-01-01'` compares dates, not text), each inserted
into the plan as an explicit `Cast` node.

**Storage.** Contiguous typed buffers plus a validity bitmap; strings are
offset-encoded (one byte buffer + a `u32` offset array, not `Vec<String>`).
Row groups of 64K rows, each carrying per-column min/max/null-count/distinct
estimate. CSV ingestion infers the narrowest type that fits, distinguishes an
unquoted empty field (NULL) from `""` (empty string), and reports the exact row
and column when a later value contradicts the inferred type.

**Parquet.** A reader written from the specification -- Thrift compact protocol,
footer metadata, pages, the encodings that turn up in practice, and Snappy, all
by hand. Loading reads the footer and nothing else: zone maps come from the
file's own statistics, and a column chunk is decoded the first time a scan asks
for one. `.load` picks the reader from the bytes, not the file name.

**In the browser.** A `wasm-bindgen` boundary crate wrapping `Engine`, with
results read as typed-array views straight out of wasm memory, and a page
showing the plan, every optimizer rewrite, per-operator timings and row counts,
the row groups and zone maps underneath, and the B+ tree an index scan descends.
A second page runs the same queries against sql.js in the same tab.
`tools/build_web.sh --serve` builds and serves both.

**Errors.** Every token, AST node and bound expression carries a byte span, so
every diagnostic points at the exact characters:

```
error[bind]: no such column `naem`
  --> line 1, column 8
  |
1 | SELECT naem FROM people
  |        ^^^^ did you mean `name`?
```

**Indexes.** Zone maps on every row group; bloom filters on every
high-cardinality column; a hand-written B+ tree on any column you ask for. The
scan picks between a full scan, a pruned scan and an index lookup -- the third
real operator choice in the engine, after hash-versus-nested-loop join and
sort-versus-top-N.

**Instrumentation.** Every operator records rows in/out, batches, exclusive wall
time and peak batch size. Scans additionally report row groups scanned, pruned,
and how many of those the bloom filters caught that the zone maps could not.
This is the data the execution view will render.

## Design decisions made so far

**Plan representation: owned tree with explicit IDs.** `enum LogicalPlan` with
`Box` children; every relation carries a `RelId` and every expression an
`ExprId`. Rewrite rules will be `fn(plan) -> Option<plan>` applied bottom-up,
and the optimizer trace will be full before/after clones. Plans are tens of
nodes, so a clone is microseconds, the UI wants whole trees anyway, and there is
no aliasing to reason about. An arena would win if this were heading for a
Cascades memo; it isn't, yet.

**Column references are `(RelId, index)`, not batch offsets.** Execution maps a
`RelId` to where that relation's columns start in the incoming batch. With a
single scan that mapping is trivially `[(rel, 0)]`, but it is the indirection
that will let join reordering move relations around without rewriting every
expression.

**Two evaluators, and they must agree.** `expr::scalar` is row-at-a-time and
obviously correct; `expr::vector` is batch-at-a-time with typed kernels and is
what actually runs queries. The scalar one was not deleted after it was
outgrown -- it is the definition of what an expression means, and every query in
the corpus is run through both and compared (`tests/evaluator_agreement.rs`).

**Timing is exclusive.** An operator stops its own clock while pulling from its
child, so per-operator times sum to the total instead of nesting.

**Narrow storage, wide arithmetic.** A column whose values fit in 32 bits is
stored as Int32 -- that is the point of having the type. But arithmetic on it is
evaluated at Int64, because 32-bit storage is an inference detail the CSV loader
chose, not a request for 32-bit arithmetic, and `id * salary` should not
overflow for reasons invisible to whoever wrote the query. Comparisons
deliberately do *not* widen: they stay in the column's own width so zone-map
bounds and (later) dictionary codes compare natively. The differential fuzzer
found this one.

**Implicit casts are recorded but not displayed.** Every coercion the binder
performs becomes a real `Cast` node, so the optimizer and the bound-plan view
can see it. User-facing renderings -- default output column names, operator
labels -- hide it, because `SELECT age + 1` should not produce a column called
`CAST(age AS INT64) + 1`.

**Batches are views; filters produce selections.** A scan hands out `Arc`-shared
pointers to the row group's own columns plus a contiguous range -- scanning a
million rows copies nothing. A filter narrows the selection rather than
rewriting every column to keep the rows that survived, so columns nothing
downstream reads are never touched. Only a projection materializes.

**Operator selection begins at joins.** A hash join needs an equality between
the two sides to hash on, so the condition is split into equi-join key pairs and
a residual; with no key pair left, the only option is a nested loop. That
residual is *not* a filter above the join: under an outer join, a row whose only
candidate fails it still has to be emitted NULL-padded, which is why the join
operator applies it rather than a `Filter`. What is still *not* decided is which
side to build -- you want the smaller one, and knowing which takes cardinality
estimates.

**Outer joins widen nullability in the scope, not just in the node.** A `LEFT
JOIN` can emit a row whose right-hand columns are all NULL even when the base
columns are `NOT NULL`, so binding marks those relations nullable for every
reference made afterwards. Without that, the flags the outer-join rewrite rules
will read are wrong -- and this was a real bug, caught by a unit test asserting
the schema rather than the rows.

**Conjunctions short-circuit, because they must.** A vectorized `AND` that
evaluated both sides over the whole batch would raise on `a <> 0 AND 100 / a > 1`
for the very rows the first conjunct rejected. So the right side is evaluated
over a sub-batch of the rows the left leaves undecided. That is also a speedup,
but it is done for correctness: `CompiledExpr::can_raise` decides when the
narrowing is mandatory versus merely worthwhile.

## The optimizer

```
qe> .trace SELECT p.name, o.product FROM people p JOIN orders o ON p.id = o.person_id
      WHERE 1 = 1 AND p.city = 'London' AND o.quantity > 1;
```

```text
initial plan:
  Project [name, product]
    -> Filter (((1 = 1) AND (city = 'London')) AND (quantity > 1))
      -> Join INNER on (id = person_id)
        -> Scan table=people columns=[id, name, age, city, department, salary, score, hired, active]
        -> Scan table=orders columns=[order_id, person_id, product, quantity, unit_price, ordered_on, shipped]

step 1 -- constant_folding at #3:
    -> Filter ((city = 'London') AND (quantity > 1))

step 2 -- predicate_pushdown at #3:
    -> Join INNER on (id = person_id)
      -> Filter (city = 'London')
        -> Scan table=people ...
      -> Filter (quantity > 1)
        -> Scan table=orders ...

step 3 -- projection_pushdown at #4:
      -> Filter (city = 'London')
        -> Scan table=people columns=[id, name, city] (6 column(s) pruned)
      -> Filter (quantity > 1)
        -> Scan table=orders columns=[person_id, product, quantity] (4 column(s) pruned)
```

**Every step is a complete plan.** Rules run bottom-up; the driver applies the
first one that fires, records the whole plan before and after, and restarts.
That is not the fastest way to run a rule set -- it is the way that makes each
step independently renderable, which is what a trace slider needs. Recording is
built in rather than added later, because retrofitting it means revisiting every
rule already written; there is no untraced path, `optimize` just discards the
trace.

Steps hold full plan clones. Plans are tens of nodes, so a clone is
microseconds, and it means the UI can jump to any step without replaying from
the start -- with no patch format to keep in sync with the plan shape.

### The rules

**Constant folding + boolean simplification.** Evaluates any subexpression that
reads no columns; simplifies `x AND true`, `x OR false`, `NOT NOT x`. Two things
it refuses to do, both about not inventing errors:

- It never folds an expression whose evaluation fails, so
  `CASE WHEN false THEN 1/0 ELSE 2 END` does not become a planning error. Folding
  is attempted and abandoned on error, which covers every case without
  enumerating them. (It *does* fold that CASE to `2`, because the dead branch is
  not evaluated at runtime either -- the fold matches what execution does.)
- It never drops an operand that could raise. `x AND false` is `false` only when
  evaluating `x` could not have failed; `BoundExpr::can_raise` gates it.

**Predicate pushdown.** Moves each conjunct as far down as it legally goes:
below a join onto one input, below an aggregate where a HAVING condition on a
grouping key becomes a WHERE, and onto a scan where it feeds zone-map pruning.
Stacked filters merge.

The outer-join rule, which is the subtle one: **a predicate may only be pushed
into a side the join preserves.** Take
`a LEFT JOIN b ON a.id = b.id WHERE b.y > 5`. Filtering `b` first removes its
low-`y` rows, so `a` rows that would have matched now find nothing -- and a LEFT
join emits those, padded with NULLs. The predicate moved down, so nothing above
removes them, and the query returns rows it should not have. Pushing into the
*preserved* side is fine: `WHERE a.x > 5` removes rows the filter above would
have removed anyway. So inner and cross joins take predicates on either side, a
LEFT join only on the left, a RIGHT join only on the right, and a FULL join
none.

Pushing a HAVING condition below an aggregate is valid for a different reason: a
grouping key has the same value for every row of its group, so filtering on it
before grouping removes exactly the rows that would have formed the groups
HAVING was about to discard. A condition mentioning an *aggregate* has no such
property and stays put.

**Projection pushdown.** Prunes columns a scan does not read. This is a
whole-plan rewrite, not a local one: whether a scan needs a column depends on
what every ancestor reads, and a subtree cannot know that. The `Rule` trait has
a `whole_plan()` flag and the driver only ever offers such rules the root --
getting that wrong would prune columns an ancestor was about to read.

`SELECT COUNT(*) FROM a CROSS JOIN b` prunes *every* column from both scans,
which turned out to be a real edge: `Batch::dense` infers its row count from the
first column and there wasn't one, so the join reported zero rows. Batches now
state their row count explicitly where columns may be absent.

### Zone maps

Every row group already carries per-column min/max/null-count, computed in the
one pass that wrote it. A predicate is evaluated over those intervals in
three-valued logic: `ALWAYS FALSE` skips the group, `ALWAYS TRUE` exists only
because `NOT` needs it, and anything unrecognised falls through to `UNKNOWN`.
Every rule is one-sided -- being wrong toward `UNKNOWN` costs time, being wrong
toward `ALWAYS FALSE` costs correctness.

NULLs need care in one direction only. A NULL never satisfies a predicate, so it
can never invalidate an `ALWAYS FALSE` verdict -- but it does invalidate
`ALWAYS TRUE`, which is why every such verdict also demands the column have no
NULLs.

The predicate reaches the scan as a *hint*: the Filter above still evaluates it,
so a bug in pruning shows up as missing rows, never as wrong ones -- and
`ExecOptions::without_pruning()` runs the same query without it, which the test
suite uses on every corpus query.

### Bloom filters

A zone map is excellent on a clustered column and useless on a scattered one:
every row group's min/max spans the whole domain, so no range test rules
anything out. A bloom filter answers a different question -- not "could a value
in this range be here" but "could *this exact value* be here" -- and it answers
it well precisely where the zone map cannot, because a scattered column is one
where most values are absent from most groups.

One filter per row group per high-cardinality column, built in the same pass
that computes the zone map. Sizing is the standard formula for a 1% false
positive rate: ~9.6 bits and 7 hashes per element, with both hashes taken from
one 64-bit hash split in half. Only columns whose distinct count exceeds half
their rows get one -- a column with twelve distinct values appears in every row
group, so its filter would answer "maybe" every time while still costing the
memory.

The direction of the one-sidedness is the whole design. A negative is certain, a
positive may be wrong. Saying "not here" about a value that *is* here would lose
rows; saying "maybe" about one that is absent only costs a scan.

Two obligations follow, and both are enforced rather than hoped for:

- **The build-time and probe-time hashes must agree.** Filters are built from
  column values and probed with literals, so `Column::hash_at` and
  `types::hash_scalar` are written against each other, and a literal from a
  different `value_class` is refused outright. An `Int64` probe against a
  `Float64` column would hash differently from an equal stored value, and the
  failure would be silent.
- **NULLs are neither stored nor found.** `col = NULL` is never true, so nothing
  would ever probe for one.

The taxi fixture carries a `medallion` column for exactly this: 200,000 values
shuffled through a million rows, so each appears about five times and every row
group's range runs end to end.

```bash
cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
    --bench benches/blooms.sql --bench-baseline no-bloom
```

| predicate | with | without | speedup | groups read |
| --- | ---: | ---: | ---: | ---: |
| `medallion = 'M0000064'` (absent, in range) | 0.02 ms | 6.44 ms | **349x** | 0 / 16 |
| `medallion IN` (three absent values) | 0.02 ms | 26.5 ms | **1251x** | 0 / 16 |
| `medallion = 'M0100000'` (present, 2 rows) | 0.86 ms | 6.41 ms | 7.5x | 2 / 16 |
| `medallion = 'M0050000'` (present, 5 rows) | 2.11 ms | 6.42 ms | 3.0x | 5 / 16 |
| `COUNT(*), AVG(fare)` on one medallion | 2.11 ms | 6.41 ms | 3.0x | 5 / 16 |
| `medallion = 'M0199999'` (the domain maximum) | 2.53 ms | 2.58 ms | 1.0x | 6 / 16 |
| `trip_id = 777777` (clustered) | 0.04 ms | 0.04 ms | 1.0x | 1 / 16 |
| `vendor = 'yellow'` (no filter built) | 13.0 ms | 13.0 ms | 1.0x | 16 / 16 |

The last three rows are the honest half. On the *clustered* column the zone map
already skips fifteen groups of sixteen, so `trip_id`'s filters -- which are
built, and cost 77 KiB per row group -- contribute nothing at all. The domain
maximum is the same story on a scattered column: a group whose max is below
`'M0199999'` cannot hold it, so the zone map is perfect there too and the
filters find nothing left to skip.

### B+ tree indexes

Zone maps and bloom filters can only *reject* a row group. An index can name the
rows. `storage/btree.rs` is a hand-written B+ tree -- node splits, separator
promotion, a linked leaf chain -- built over one designated column by
`.index <table> <column>`, since there is no DDL yet and which column deserves
one is not a judgement the engine should make on its own.

Values live only in the leaves, and the leaves are chained left to right, so a
range scan descends once and then walks the chain. That is the property that
makes this a B+ tree rather than a plain B-tree, and the reason
`WHERE x BETWEEN a AND b` is a single traversal. Splitting differs by level and
the difference matters: a leaf split *copies* its first key up, because every
value must remain in a leaf for the chain to be complete, while an internal
split *moves* the middle key up, because a separator is not a value and
duplicating it would put the same routing decision in two places.

Nodes live in an arena and refer to each other by index. That sidesteps the
parent-pointer aliasing that makes tree surgery painful in Rust, and it makes
the tree trivially serializable for the index visualizer the UI will want.

**NULLs are not in the tree.** Every predicate an index scan can serve -- `=`,
`<`, `<=`, `>`, `>=`, `BETWEEN` -- is UNKNOWN, never TRUE, when the column is
NULL, so leaving those rows out cannot lose an answer. The obligation lands on
the planner: it must never serve `IS NULL` from the index, because that
predicate is true for exactly the rows the tree omits. Two records in
`indexes.slt` and a unit test exist to prove it does not.

**Rows come back in table order, not key order.** The tree walks its leaves by
key; for a scattered column that is a completely different order from the
table's, and a query with no `ORDER BY` would silently start returning rows
differently depending on whether an index happened to exist. An index scan is
meant to be a faster route to the same answer, and "the same answer" includes
the order nothing pinned down.

### Choosing the index, on a real number

An index scan wins on a needle and loses badly on a haystack, because gathering
scattered rows gives up the sequential access that makes a columnar scan fast.
So the choice has to be made -- and it is made on the true matching row count
rather than an estimate. `range_limited` walks the leaves with a cap and returns
`None` the moment too many rows qualify; the planner takes that as "scan
instead". A bad guess costs a bounded amount of abandoned work rather than a bad
plan, and when several indexes apply the one naming the fewest rows wins.

```bash
cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
    --index trips.fare --bench benches/indexes.sql --bench-baseline no-index
```

The cap is 5% of the table, measured rather than guessed. Same query shape
throughout -- `SELECT COUNT(*) FROM trips WHERE <predicate>` -- so the only
variable is how many rows qualify, with the cap temporarily lifted to 100% so
the index is forced to run past the point where it stops paying:

| rows matched | share | speedup |
| ---: | ---: | ---: |
| 963 | 0.1% | 13.7x |
| 3,675 | 0.4% | 6.1x |
| 16,509 | 1.7% | 2.1x |
| 34,810 | 3.5% | 2.0x |
| 58,819 | 5.9% | 1.0x |
| 91,832 | 9.2% | 0.8x |
| 126,500 | 12.7% | 0.7x |
| 253,360 | 25.3% | 0.6x |
| 452,011 | 45.2% | 0.5x |

The crossover is just under 6%. The cap sits below it rather than on it: being
slightly conservative costs a fraction of the win on one query, while being too
aggressive costs a multiple on every query past the line, and the shape of that
curve is dataset-dependent in a way the margin is meant to absorb. Queries past
the cap land within noise of the baseline -- `fare > 20` is 1.27 ms against
1.30 ms -- which is what a declined index is supposed to cost.

On the scattered column the index beats the bloom filters by an order of
magnitude, because it skips the surviving groups' rows too rather than just the
groups:

| `medallion = 'M0050000'` | time |
| --- | ---: |
| index scan | 0.02 ms |
| bloom-filtered scan | 2.11 ms |
| full scan | 6.42 ms |

Building it is not free: 198,612 keys over a million rows takes about 520 ms
and 20 MiB, because the tree is built by repeated insertion rather than
bulk-loaded -- which is what exercises the split path, and is several times
slower than sorting first would be.

## Parquet

A Parquet reader written from the format specification: Thrift compact protocol,
footer metadata, pages, encodings and Snappy, all by hand. No Arrow, no
`parquet` crate -- the engine crate still has zero dependencies.

```text
  PAR1 <column chunks, page by page> <FileMetaData> <footer length> PAR1
```

The footer is last so a writer can stream data without knowing offsets in
advance, and it is what makes the format worth reading: schema, row-group
boundaries and per-column statistics, all available before a byte of data is
decompressed.

### Thrift first

Parquet's metadata is a Thrift-serialized struct, so a Parquet reader is a
Thrift reader first (`storage/parquet/thrift.rs`). Two details carry most of
the weight.

**Field ids are deltas, and the delta is per struct.** A field header carries
the difference from the previous id in four bits, which is why most headers cost
one byte. Nested structs therefore need a stack -- descending resets the running
id, returning must restore it. Get that wrong and the ids come out plausible and
silently wrong, which is worse than a parse error.

**The skip path matters more than the read path.** `min_value`/`max_value`
replaced `min`/`max`; column indexes and bloom-filter offsets arrived later.
A reader that meets a field it does not know must step over exactly its bytes
and carry on, so every struct loops over whatever it is handed rather than
expecting a layout.

### Encodings

| | |
| --- | --- |
| schemas | flat only -- lists, maps and structs refused by name |
| pages | v1, v2, dictionary |
| encodings | PLAIN, PLAIN_DICTIONARY, RLE_DICTIONARY, RLE, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT |
| compression | UNCOMPRESSED, SNAPPY |
| types | BOOLEAN, INT32, INT64, INT96, FLOAT, DOUBLE, BYTE_ARRAY, FIXED_LEN_BYTE_ARRAY, with both the `LogicalType` and `ConvertedType` annotations |

A column is not a buffer of values; it is a buffer of values in whichever
encoding the writer thought cheapest, and that choice varies by library, by
version and by column. So a reader that handles only PLAIN works on files it
wrote itself and fails on everyone else's. INT96 is deprecated and a decade old
and still turns up from Hive and Spark, so it is read too.

Snappy is decompressed by hand, in its raw form -- Parquet stores pages without
Snappy's stream framing, so a framed decoder fails on the first byte. The
back-references may name an offset shorter than their own length, which is not
corruption to defend against but how runs are encoded: `x` three hundred times
is a literal `x` followed by copies of length 64 at offset 1.

### Nothing is decoded until a scan asks

Loading a Parquet file reads the footer and stops. Every row group's zone map
comes out of the file's own statistics, so pruning works before anything has
been read; a column chunk is decoded the first time a scan asks for it and
cached from then on. `RowGroup` holds a `ChunkSource` behind a trait rather than
a Parquet type, so the browser's eventual range-request loader slots in without
touching a scan.

```text
qe> .rowgroups trips
loaded `trips`: 1000000 rows, 9 columns, 16 row groups, 22.7 MiB compressed, columns not yet decoded
  row group 1: 65536 rows, 1.2 MiB, 0/9 column(s) decoded
qe> SELECT COUNT(*) FROM trips WHERE fare > 45;
  row group 1: 65536 rows, 1.7 MiB, 1/9 column(s) decoded
```

The million-row taxi fixture, the same data as CSV and as Parquet:

| | CSV | Parquet |
| --- | ---: | ---: |
| open the file | 1.12 s | **0.04 s** |
| resident after loading | 65.8 MiB decoded | 22.7 MiB compressed |
| first query touching one column | 2.2 ms | 7.4 ms |
| first query touching all nine | 35.2 ms | 61.2 ms |
| every query after that | identical | identical |
| a query pruned to nothing | 0.086 ms | 0.082 ms, nothing decoded |

Parquet opens 28x faster and pays the difference back on first touch, per
column, once. A query whose row groups are all pruned costs the same either way
-- but on Parquet the data was never read, which with CSV is not a thing that
can be arranged.

### The statistics tradeoff

The cost model wants distinct counts, histograms and most-common values, and
those cannot be read from a footer -- they need the values. Computing them at
registration would hand back exactly the laziness the format exists to provide.

So they are built from a **sample of row groups**: all of them when the data is
already in memory, the first one when it is not. Bounds and null counts still
come from every row group's metadata, exactly and for free. Distinct counts are
projected from the sample, weighted by how distinct the sample was -- a sample
where every row held a new value scales fully, one where almost none did barely
scales at all. A `vendor` column with three values sampled from one row group in
sixteen must not come back with forty-eight.

### Two bugs the fixture matrix found

`tools/gen_parquet.py` writes the same thousand rows a dozen ways -- each
encoding, both page versions, both codecs, one row group and eight, dictionary
on and off. Any disagreement between those files is a decoder bug rather than a
data difference, which is what makes a matrix worth the trouble.

**The last miniblock of a delta block is padded, and the padding must be
consumed.** `DELTA_BYTE_ARRAY` stores prefix lengths, then suffix lengths, then
bytes -- all three back to back. Stopping at the last real value left the bit
reader mid-miniblock, so the suffix header was read out of the middle of the
prefix data. It failed loudly on a 1000-row column and would have succeeded
quietly on one whose values happened to land on a miniblock boundary.

**RLE-encoded boolean *values* carry a four-byte length prefix; v2 definition
levels do not.** Both are RLE hybrid streams in the same page. The levels' length
is in the page header, so they need none; the values run to the end of the page,
so they carry one. Reading the values without skipping it produces no error at
all -- the prefix decodes as a plausible run header and every boolean comes out
shifted. I found it by dumping the page bytes rather than by re-reading the
spec, which was the faster route by a wide margin.

### The corpus, answered from Parquet

`tests/parquet_corpus.rs` redirects every `load` in the sqllogictest corpus to
the Parquet twin of the same table and runs the whole thing: **974 records**,
every join, aggregate, window, subquery, NULL case and index scan, answered from
Parquet and compared against the expectations SQLite produced from the CSV.

That is a stronger claim than "the reader decodes the fixtures". It says the two
loaders are interchangeable -- same types, same nullability, same NULLs, same
ordering, everything a query can observe. A bug in a rarely used encoding shows
up as a wrong answer to an ordinary query instead of as a difference nobody
looks at.

pyarrow generates the fixtures and is the oracle here, exactly as SQLite is the
oracle for SQL semantics: a mature independent implementation, used as a tool
rather than as a dependency. Nothing in `crates/` knows it exists.

## Merge join, and the aggregate that streams

Step 7 of the build order reads *"joins: hash join, then nested loop **and
merge**"*. The merge join was skipped and stayed skipped, along with the
sort-based aggregate the operator list also names -- because neither can be
*chosen* without a way to answer "is this input already sorted, and by what?".

`exec::ordering` answers it. An ordering survives a Filter and a Limit, is
remapped through a Project, is renumbered through a SubqueryAlias, and comes
from exactly one place: an explicit `Sort`. Nothing else claims one. The module
is conservative on purpose -- only bare column references count as sort keys,
because claiming an ordering that does not hold is not a slow plan, it is a
merge join walking past rows it will never look at again.

### What each operator is for

A **merge join** walks both inputs once in key order and never looks back. Its
memory is one *run* -- the rows sharing a key -- rather than a whole build side,
and its output arrives sorted, which can make an `ORDER BY` above it free.

A **streaming aggregate** holds one accumulator set instead of one per group,
because sorted input puts every group's rows together and a group is finished
the moment the key changes. Over the taxi fixture's 198,612 medallions that is
one accumulator set against 198,612.

Both are chosen automatically when the ordering is already there:

```text
qe> SELECT COUNT(*) FROM (SELECT trip_id, fare FROM trips ORDER BY trip_id) a
      JOIN (SELECT trip_id, vendor FROM trips ORDER BY trip_id) b ON a.trip_id = b.trip_id;

StreamAggregate  rows=1
  -> MergeJoin   rows=1000000
    -> Sort      rows=1000000
    -> Sort      rows=1000000
```

Sort the two sides differently and the plan goes back to a hash join, because
two inputs sorted opposite ways are not merge-joinable however sorted they are.

### And they usually lose

The honest measurement. `--bench-baseline merge-join` and `stream-aggregate`
*force* the alternative, which means paying for the sorts -- and that is the
real comparison, because almost nothing in a plan arrives sorted on its own:

| | hash | sort-merge | |
| --- | ---: | ---: | ---: |
| `trips JOIN boroughs`, five-row dimension | 124 ms | 366 ms | **0.34x** |
| the same, feeding a `GROUP BY` | 231 ms | 490 ms | 0.47x |
| `LEFT JOIN` to the same dimension | 124 ms | 363 ms | 0.34x |

| | hash | streaming | |
| --- | ---: | ---: | ---: |
| `GROUP BY vendor` (3 groups) | 119 ms | 254 ms | **0.47x** |
| `GROUP BY borough, vendor` (15) | 211 ms | 566 ms | 0.37x |
| `GROUP BY pickup_date` (336) | 63 ms | 88 ms | 0.71x |
| `GROUP BY medallion` (198,612) | 265 ms | 491 ms | 0.54x |

Hash wins every one, by 1.7x across the join set and 1.8x across the aggregate
set. That is the correct result and worth stating plainly: sorting a million
rows to meet a five-row dimension table is a bad trade, and a hash table over
198,612 groups is still cheaper than sorting a million rows to avoid it.

What the streaming aggregate buys is not time but **memory**, and the benchmark
cannot show that. What the merge join buys is an ordered output, which the plan
above spends on nothing. Both become the right choice when the sort is already
paid for by something else -- which is exactly when the planner picks them, and
never otherwise.

Note the third aggregate row: at 336 groups the gap narrows to 0.71x, and it is
the only one where the sort is nearly worth it. The curve is going the right
way; the fixture just never has enough groups relative to rows.

### One consequence worth naming

An aggregate with no `GROUP BY` has one group, so the ordering requirement is
empty and *everything* satisfies it -- every ungrouped aggregate now streams.
That is an improvement rather than an accident: the hash implementation was
building a one-entry table and hashing an empty key once per row. The operator
is called `StreamAggregate` rather than `SortAggregate` for that reason; sorted
input is how its groups usually arrive contiguously, not what it requires.

### Two bugs, one of them silent

**A run of equal keys lost every duplicate after the first.** Loading the
right's run advances the right cursor past it, so a second left row with the
same key compared against whatever came *next* and was declared unmatched.
Three left rows against two right rows returned two pairs instead of six. The
fix is to check the buffered run before the cursor; the test that pins it is
`a_run_of_equal_keys_pairs_with_every_row_of_the_other_run`.

**Both new operators spun forever.** `next` looped while the buffer held fewer
than a full batch; the fill routine looped while the buffer was *empty*. Once
one row was buffered neither made progress, and the agreement test ran for ten
minutes before I killed it. Both now take the target count as an argument, so
the two loops share one condition.

### Checked against the implementations they replace

`tests/evaluator_agreement.rs` now runs the corpus **nine** ways. Forcing
sort-merge joins and forcing streaming aggregates each run all 938 queries
through a completely different operator and demand the same rows -- every join
type, every NULL case, every outer join.

38 comparisons are skipped, and the reason is worth recording: `LIMIT` without a
total `ORDER BY` has no unique answer. `GROUP BY city LIMIT 3` over seven cities
returns whichever three come first, and a streaming aggregate emits in key order
where a hash aggregate emits in insertion order. Both are correct SQL. The skip
is conservative -- any `LIMIT` at all under a reordering strategy, since
`ORDER BY x LIMIT 3` with ties in `x` is just as under-determined.

## In the browser

Running at
**[dhanushkrishna4.github.io/Query-engine-in-the-browser](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/)**,
or locally against your own build:

```bash
tools/build_web.sh --serve      # http://localhost:8137
```

`crates/wasm` is the only crate that knows JavaScript exists. `engine` still has
no dependencies and no host assumptions; everything wasm-specific -- the
bindings, the JSON, the panic hook, the clock -- lives on the other side of that
boundary. The page is four files and a 939 KiB `.wasm`: no bundler, no
`node_modules`, `--target web` emitting an ES module a plain
`<script type="module">` can load.

### Results do not cross as values

Handing a million rows to JavaScript one value at a time would cost more than
running the query. Instead the result keeps its columns in wasm memory as dense
typed buffers and hands out *pointers*; the page builds `Int32Array`,
`Float64Array` and `BigInt64Array` views straight onto the module's memory and
reads them without a copy.

```js
const buffer = wasm.memory.buffer;            // re-read every time
const values = new Float64Array(buffer, outcome.valuesPtr(i), outcome.len(i));
```

One rule comes with that, and it is the one that bites: **a view is valid only
until wasm allocates again**, because growing the memory detaches every
`ArrayBuffer` built on the old one. So `memory.buffer` is re-read for each
column and no view outlives the function that made it.

Three details are decided at the boundary rather than fudged:

- **A column with no NULLs hands back a null pointer** instead of a bitmap of
  all ones, so the page's fast path is the common one.
- **Strings cross as a byte buffer plus `u32` offsets**, decoded with
  `TextDecoder` -- the layout the engine already stores them in, so there is
  nothing to convert.
- **Decimals cross as text.** JavaScript has no 128-bit integer and its `number`
  would round, which is the one thing a decimal column exists to prevent.

Everything that is *not* data -- schemas, plans, optimizer traces, the operator
statistics tree -- crosses as JSON. It is small, structural and read once per
query, which is what `serde_json` is for and why `serde` is on the allowed list.
It is never on the data path.

### The engine had no clock

`std::time::Instant` panics on `wasm32-unknown-unknown`: WebAssembly has no
clock of its own, only whatever the host hands in, and timing had been reporting
zero on wasm since step 8. The boundary now installs one -- a `fn` pointer in a
`OnceLock`, so the engine gains a clock without gaining a dependency.

Getting it wrong was instructive. The first version looked up `performance.now`
and called it with a null receiver, which browsers reject outright: it is a
method, not a free function. There was no error anywhere, only every timing
coming back as exactly `0.000 ms` -- including a 100,000-row cross join, which
is what gave it away. It also does the lookup once now rather than three
property reads per `Timer::start`, and `Timer::start` runs around every
operator's every batch.

`performance.now()` is coarsened by the browser, so timings land on 0.1 ms
boundaries: 0.700 ms for a small query, 49.700 ms for that cross join. Real
numbers, coarse ones -- sub-microsecond per-operator timings on wasm are noise.

### What the page shows

Four tabs, all fed by instrumentation the engine has carried since step 8 rather
than added for a UI:

| | |
| --- | --- |
| **results** | the grid, read out of wasm memory |
| **plan** | the optimized plan, the same plan with every leaf's resolved type, and the plan as the binder produced it |
| **pipeline** | the operator tree with rows in and out, each node's *exclusive* share of the time as a bar, the batches it handed upward, row groups read versus pruned, and the estimate beside reality with its q-error |
| **optimizer trace** | a slider over the rewrites, one at a time, with the subtree the rule fired at highlighted in both plans |
| **storage** | every row group's zone map, bloom filters, and -- for a Parquet table -- how many of its columns have actually been decoded |
| **index** | a B+ tree drawn level by level, and the path a probe takes down it |

### Four views, and what each is actually showing

**The pipeline** draws one dot per batch an operator handed upward. That is not
decoration: the engine is batched at 2048 rows, and a scan over the eight row
groups of the Parquet demo table really does emit eight batches while the
aggregate above it collapses them to one. The bar under each operator is its
*exclusive* share of the time, so the shares sum to the total instead of
nesting.

**The trace slider** steps through the rewrites one at a time. Each step carries
the `RelId` the rule fired at -- that field has existed since step 11 with a
comment saying it was for exactly this -- so the subtree that changed is
highlighted in both plans rather than left to be found by diffing two blocks of
text. Plans cross the boundary as trees now, not strings.

**The storage inspector** is where the last three steps become visible at once.
Opening it on the Parquet table after running
`SELECT id, token FROM metrics WHERE id >= 300 AND id < 340`:

```text
metrics · 1,000 rows · 8 row groups · 59.0 KiB · lazily decoded from Parquet

row group 0   128 rows    10/10 columns decoded     id  int32  min 0    max 127
row group 1   128 rows     0/10 columns decoded     id  int32  min 128  max 255
row group 2   128 rows     2/10 columns decoded     id  int32  min 256  max 383
row group 3   128 rows     0/10 columns decoded     id  int32  min 384  max 511
...
```

One row group read out of eight, and in it two columns of ten. The bounds came
from the file's footer, so the seven skipped groups were never touched. Group 0
is fully decoded because registration sampled it to build the cost model's
histograms -- the tradeoff from step 16, visible rather than described.

**The index visualizer** draws the tree and the descent. A level wider than the
cap is sampled evenly across its key range -- an index over a million rows has
tens of thousands of nodes and no screen wants them -- but the nodes on the
traversal path are always kept, whatever the sampling, or the picture would show
a route through nodes it had thrown away.

```text
                    tok-0000003…ok-00000960
                            30 keys
        ┌───────┬───────┬───────┼───────┬───────┬───────┐
    32 keys  32 keys  32 keys  32 keys  32 keys  32 keys      showing 9 of 31
                            ▲
                  tok-0000…00000511, 32 keys
```

Two levels, one node visited per level, one row found. A probe is parsed against
the indexed column's own type before it is used, because a text box gives
strings and comparing `"42"` lexicographically against integers would draw a
path that is simply a lie.

Errors arrive already rendered, caret and all, so the browser shows what the
terminal would:

```text
error[bind]: no such column `naem`
  --> line 1, column 8
  |
1 | SELECT naem FROM people
  |        ^^^^ did you mean `name`?
```

Datasets are fetched and cached in IndexedDB exactly as they arrived -- so a
Parquet file stays compressed on disk and is still decoded lazily by the engine.
A browser that refuses the cache (a private window, site data blocked) gets the
engine anyway: `openDb` resolves to `null` and the fetch simply happens again.

### Verified in a real browser

Twelve tests cover the boundary's conversions natively under `cargo test` --
buffer layouts, validity bitmaps, string offsets, the truncation cap, the
rendered errors. That is possible because the logic is split from the
`#[wasm_bindgen]` surface: `JsValue` cannot be constructed off a wasm target, it
aborts, so the JS methods are one-line adapters over plain-Rust ones.

The page itself was then driven in Chrome -- queries run, results compared
against the native CLI value for value, the trace and pipeline panels inspected,
the error path checked. The same join returns the same three rows in the same
order on both sides.

### Deliberately not yet

- **Results are capped at 10,000 rows.** The engine computes every row -- the
  count and the timings are of the whole query -- but a grid cannot show a
  million, and the page says how many it is showing.
- **It runs on the main thread**, so a long query freezes the tab. A worker is
  the fix, and the clock already reaches `performance` through the global rather
  than through `window` so that it will work there.
- **SIMD128 is enabled but unverified.** `.cargo/config.toml` passes
  `+simd128`; whether v128 instructions are actually emitted needs
  `wasm-objdump`, which is not installed here. Treat it as unmeasured.
- **The pipeline shows batches, not their contents.** Watching a selection
  vector narrow batch by batch would need per-batch events rather than
  per-operator totals, which is a change to the `Operator` trait rather than to
  the page.
- **The index visualizer samples wide levels.** Panning and zooming a full tree
  is a different piece of software; this draws a faithful sample with the path
  intact and says how many nodes it stood in for.

## Against sql.js

**[Run it yourself](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/bench.html)**
-- the numbers below are from one machine and yours will differ. Locally:

```bash
tools/build_web.sh --serve      # then open /bench.html
```

sql.js is SQLite compiled to WebAssembly: a row store with twenty-five years of
work behind it, in the same tab, on the same clock, over identical rows. Both
are timed with `performance.now()` and reported as the minimum of five runs
after a warm-up -- the same methodology as the native benchmarks.

**Every query's results are compared between the two engines before its timing
is believed.** A benchmark that is faster because it computed something else is
not a benchmark, and running two independent SQL implementations over the same
data makes that check nearly free. The page says so on every row, and says it
loudly when the answer is no.

200,000 rows, eight columns, about one tip in fifty NULL:

| query | this engine | sql.js | ratio | rows |
| --- | ---: | ---: | ---: | ---: |
| `COUNT(*)` over everything | 11.0 ms | 3.9 ms | **0.35x** | 1 |
| four aggregates, no grouping | 30.6 ms | 73.5 ms | 2.40x | 1 |
| `COUNT(*) WHERE fare > 45` | 1.3 ms | 47.6 ms | **36.6x** | 1 |
| `GROUP BY vendor` (3 groups) | 74.7 ms | 257.2 ms | 3.44x | 3 |
| `GROUP BY borough, vendor` | 165.2 ms | 479.8 ms | 2.90x | 15 |
| `GROUP BY day` (365 groups) | 45.2 ms | 272.5 ms | 6.03x | 365 |
| `WHERE borough = 'Bronx'` | 12.0 ms | 34.0 ms | 2.83x | 1 |
| `WHERE tip IS NULL` | 2.4 ms | 28.8 ms | 12.0x | 1 |
| `WHERE fare - tip > 40` | 5.6 ms | 42.5 ms | 7.59x | 1 |
| selective, returning rows | 3.4 ms | 35.6 ms | 10.5x | 1,800 |
| `ORDER BY fare DESC LIMIT 10` | 13.3 ms | 43.0 ms | 3.23x | 10 |
| `DISTINCT borough, vendor` | 100.5 ms | 89.4 ms | **0.89x** | 15 |
| join to a dimension table | 177.4 ms | 342.2 ms | 1.93x | 2 |
| point lookup by `id` | 0.2 ms | 25.7 ms | 128x | 1 |
| a narrow `id` range | 0.4 ms | 19.1 ms | 47.8x | 1 |
| **with an index on `id` in both** | | | | |
| point lookup by `id` | 0.2 ms | &lt;0.1 ms | *too fast to time* | 1 |
| a narrow `id` range | 0.3 ms | &lt;0.1 ms | *too fast to time* | 1 |

Twelve of the fourteen measurable queries went this way. The split is the one
the architectures predict, and the two losses are the informative rows.

**`COUNT(*)` loses at 0.35x.** SQLite does not read the table for it; it walks
a B-tree counting entries. This engine reads a column. There is a cheap answer
here -- a scan with no predicate and no projection could return `num_rows` from
the catalog -- and not having it is a gap, not a law of physics.

**`DISTINCT` loses at 0.89x**, which is the allocation-per-row problem written
down in the gaps section three steps ago: a row's identity is built as a tuple
of values, so every text column costs a `String`. SQLite's row store already has
the row.

**The point lookups are the honest headline.** Unindexed, this engine wins by
50-130x, because a zone map over sorted `id` skips almost every row group while
SQLite scans. Give both an index and SQLite answers below the resolution of
`performance.now()` -- a B-tree descent on a row store returns the row itself,
while this engine descends its own B+ tree and then gathers from column
chunks. Both are "too fast to measure" at this size, which is the honest way to
report a number the clock cannot see.

Loading is not like for like and is reported anyway: about 1.0 s here to parse
200,000 rows of CSV against about 0.9 s for sql.js to run 200,000 prepared
`INSERT`s in one transaction. It is what each actually pays before it can answer
anything.

### Two bugs the benchmark found

Running a second engine over the same data found things the test suite had not.

**A typed-array alignment bug in the boundary.** Result columns were stored as
`Vec<u8>` of little-endian bytes, and `Vec<u8>` is aligned to 1 -- but
`new Int32Array(buffer, ptr, n)` throws unless `ptr` is a multiple of four.
Whether it threw depended on where the allocator happened to put the buffer, so
it worked in the app for a week and failed here on one query. Columns now live
in `Vec<i32>` / `Vec<i64>` / `Vec<f64>`, which carry their element's alignment
by construction, and a test asserts every buffer's pointer is aligned for the
view that will be built on it.

**A benchmark query that was not a query.** `ORDER BY fare DESC LIMIT 10` has no
unique answer when fares repeat: each engine may pick different rows among the
ties and both are right. The comparison caught it as a disagreement, which is
exactly what it is for -- the fix is `ORDER BY fare DESC, id`, and a query that
does not pin its order has no business being compared row for row.

A third thing was not a bug but was just as misleading: with an index, SQLite's
lookups came back as `0.00 ms`, and dividing by that produced a confident
`0.00x`. Anything under `performance.now()`'s coarsened resolution is not a
small measurement, it is no measurement, and the page now says so instead of
printing a ratio.

## Deploying

Live at
**[dhanushkrishna4.github.io/Query-engine-in-the-browser](https://dhanushkrishna4.github.io/Query-engine-in-the-browser/)**.

The site is static: a `.wasm`, four files, the sample data and sql.js. Nothing
runs on a server, so there is no server to deploy -- `tools/build_web.sh`
produces a directory and GitHub Pages serves it.

Two workflows in `.github/workflows/`:

**`ci.yml`** runs on every push and pull request: clippy with `-D warnings`, the
whole test suite, a `wasm32-unknown-unknown` build, the sqllogictest corpus
through the CLI, and `gen_expected.py --check` so a divergence from SQLite fails
the build rather than sitting in a stale file. It also asserts the engine crate
still has no dependencies:

```yaml
deps=$(cargo tree -p engine --edges normal --depth 1 | tail -n +2)
if [ -n "$deps" ]; then
  echo "the engine crate grew a dependency:" >&2
  echo "$deps" >&2
  exit 1
fi
```

That constraint has shaped the whole project. A rule nobody checks is a rule
that quietly stops being true, so it is checked.

**`pages.yml`** builds the site and deploys it. It also greps for absolute
paths, because a GitHub project site is served from
`https://user.github.io/<repo>/` and a single `src="/pkg/qe.js"` would work
locally and 404 there. Verified by serving `web/` under a `/query-engine/`
prefix and running the app and the benchmark from it -- both load, both query,
and the site root 404s, which is how you know the prefix was real.

To turn it on: push to `main`, then set **Settings → Pages → Source** to
*GitHub Actions*. Nothing else is configured; `web/.nojekyll` keeps Jekyll from
processing the output.

## Ordering, DISTINCT, set operations and windows

### Sorting, and the heap that usually replaces it

```
qe> .plan SELECT name FROM people ORDER BY age LIMIT 2;

Project [name]
  -> Limit fetch=2
    -> Sort [order_by_1 ASC NULLS FIRST]
      -> Project [name, age AS order_by_1]
        -> Scan table=people columns=[name, age] (7 column(s) pruned)
```

Two things are happening there. `age` is not selected, so binding appends it as
a hidden column and a projection trims it off again -- the standard way to make
`ORDER BY` work on something the query does not return. And that projection then
sits between the `LIMIT` and the `Sort`, hiding one from the other, which is why
there is now a **limit pushdown** rule: a projection emits one row per input row
in the same order, so taking the first `n` before or after it is the same thing.
With the limit directly above the sort, the physical planner builds a bounded
top-N heap instead.

Pushing a limit into a *scan* needs no rule at all: `LimitExec` stops pulling,
and the scan stops being asked.

**NULL placement.** SQL leaves it implementation-defined. This engine follows
SQLite -- NULLs first ascending, last descending -- because SQLite is the
differential oracle. PostgreSQL chose the opposite, so a query relying on either
is relying on something the standard does not promise.

**Spilling.** Everything sorts in memory. The shape is the one an external sort
would use -- accumulate, order an index vector, gather -- so run generation and
a k-way merge would slot in without the comparison logic changing. A sort larger
than memory fails rather than getting slow.

### Set operations

All four share one idea: a row's identity is the tuple of its values, and
**NULL counts as equal to NULL** -- as it does for `GROUP BY` and `DISTINCT`,
and as it does not for a join key or a comparison. A set operation asks "is this
the same row?", not "are these values equal?".

`UNION ALL` never has to identify anything, so it streams both inputs straight
through. `INTERSECT` and `EXCEPT` default to removing duplicates; `ALL` makes
them multiset operations keeping `min(left, right)` and `max(0, left - right)`
copies, which is why the right side is *counted* rather than collected into a
set.

SQLite has no `INTERSECT ALL` or `EXCEPT ALL`, so those two are pinned by hand
in `setops.slt` rather than compared. Writing the expected counts out by hand
is also how I got one of them wrong and the engine right.

### Window functions

```
qe> SELECT name, score, RANK() OVER (ORDER BY score),
                        SUM(score) OVER (ORDER BY score) FROM people;
```

The shape is always: sort by `(partition keys, order keys)`, walk partitions,
find *peer groups* -- runs equal on the order keys -- and compute each function
over each row's frame.

Peer groups are where the interesting semantics live. `RANK` ties on them and
then skips, `DENSE_RANK` ties and does not, and the **default frame** runs to
the end of the current row's peers -- so `SUM(x) OVER (ORDER BY y)` gives every
row with the same `y` the same running total. Writing `ROWS` instead counts
rows, and the ties diverge. With no `ORDER BY` there is nothing to run along, so
the frame is the whole partition.

A whole-partition frame is computed once per partition and a running frame
accumulates in one pass; any other frame is recomputed per row, which is
`O(n * w)` and the obvious place for a sliding accumulator when a query makes it
matter.

Functions with different `OVER` clauses need different sorts, so each distinct
clause becomes its own `Window` node and they stack. A window function's column
is addressed as `(node, position within the node)` rather than by absolute
position -- the absolute position depends on how wide the input is, and
projection pushdown changes exactly that.

## What step 14 bought

`--bench-baseline no-top-n` forces a full sort where the engine would use a
heap, so the column is what top-N bought:

| query | top-N | full sort | speedup |
| --- | ---: | ---: | ---: |
| `ORDER BY fare DESC LIMIT 10` | 20.0 ms | 98.1 ms | 4.9x |
| the same with the key not selected | 20.1 ms | 98.7 ms | 4.9x |
| `LIMIT 5000` | 28.8 ms | 97.9 ms | 3.4x |
| two keys, `LIMIT 20 OFFSET 100` | 72.2 ms | 448.5 ms | **6.2x** |
| no limit -- nothing to choose | 0.81 ms | 0.79 ms | 1.0x |

The first measurement of that set came in at 2.4x, because the heap materialized
every candidate row before deciding whether to keep it -- a million rows built
for ten that survive. Deciding on the keys alone and building the row only on
admission doubled it.

Absolute times worth naming: `SELECT DISTINCT pickup_borough, vendor` over a
million rows takes 190 ms to produce fifteen. It builds a key tuple per row,
allocating a `String` per text column, which is the same trap the string
comparison and the join probe each hit in turn. A hash of the row rather than
its values is the fix, and it is not done.

## Subqueries

```
qe> .trace SELECT name FROM people
      WHERE EXISTS (SELECT 1 FROM orders
                    WHERE orders.person_id = people.id AND orders.quantity > 1);
```

```text
initial plan:
  Project [name]
    -> Filter EXISTS <Project [1]>
      -> Scan table=people ...

step 1 -- decorrelate at #4:
  Project [name]
    -> Join SEMI on (person_id = id)
      -> Scan table=people ...
      -> Filter (quantity > 1)
        -> Scan table=orders ...

step 2 -- projection_pushdown at #5:
      -> Scan table=people columns=[id, name] (7 column(s) pruned)
      -> Filter (quantity > 1)
        -> Scan table=orders columns=[person_id, quantity] (5 column(s) pruned)
```

A correlated `EXISTS` -- conceptually "for each outer row, run this query" --
collapses into one join, and the quadratic disappears. The predicate that
mentions the outer query becomes the join condition; the one that does not stays
a filter on the right input.

### Two passes, both mandatory

A subquery expression has no execution strategy of its own, so one of two rules
has to remove every one of them. They run even when the optimizer is otherwise
switched off, which is what keeps "run it unoptimized and compare" a usable
test.

**`decorrelate`** turns `EXISTS` and `IN` in a WHERE clause into semi- and
anti-joins. A semi-join emits each left row *at most once*, which is the whole
point: `EXISTS` asks whether a match exists, not how many, so a row with three
matches must not become three rows.

**`evaluate_subqueries`** handles everything else by running it, which is only
possible when the subquery is uncorrelated. A scalar subquery becomes the value
it returned -- NULL over no rows, an error over more than one. An uncorrelated
`IN` becomes an ordinary literal list, which already implements the three-valued
rule.

Anything neither can handle is reported by name at plan time rather than
reaching an evaluator that cannot run it: a correlated subquery outside a WHERE
clause, one under an `OR` (a semi-join *filters*, and there is no way to OR a
filter with something else), or one whose plan has a `GROUP BY` or `LIMIT` in
the way (lifting a correlated predicate across those changes what it filters).

### The rule that makes `NOT IN` different from `NOT EXISTS`

`NOT EXISTS` is about whether any row came back, so it is never unknown and
always becomes an anti-join.

`NOT IN` is a comparison, and a NULL on either side makes it unknown rather than
true. `x NOT IN (SELECT y ...)` where some `y` is NULL matches **no rows at
all** -- not "every row where x differs". An anti-join would return those rows,
so `NOT IN` is only rewritten when neither side can be NULL. Otherwise the
values are materialized and the three-valued rule applies directly.

A related case the fuzzer found: `x NOT IN (empty set)` is TRUE even for a NULL
`x`, because there is nothing for it to fail to match. Both evaluators returned
NULL. Unreachable from a literal list -- the parser requires at least one item --
but perfectly reachable once a subquery supplies it.

### When a join is the wrong answer

For an uncorrelated subquery both routes work, and which is faster depends
entirely on how much comes back. Measured on a million rows
(`--bench-baseline no-decorrelation`):

| query | semi-join | materialized | speedup |
| --- | ---: | ---: | ---: |
| `IN (...)` returning ~2 values | 40.1 ms | 39.9 ms | 1.0x |
| `IN (...)` returning ~2,400 values | 23.3 ms | 377 ms | 16.2x |
| `IN (...)` returning ~26,000 values | 22.5 ms | 2,554 ms | **113.6x** |
| `NOT IN (...)`, non-nullable, ~2,400 values | 36.1 ms | 387 ms | 10.7x |
| uncorrelated `EXISTS` | 10.2 ms | 10.3 ms | 1.0x |

The first run of that benchmark had the two-value case at **0.4x** and the
uncorrelated `EXISTS` at 0.8x -- decorrelating them was a *pessimization*, since
a hash build and probe costs more than two vectorized comparisons, and an
`EXISTS` with nothing to join on is just a constant. So the rule now consults
the statistics: an uncorrelated `IN` is only decorrelated when the subquery is
estimated to return more than sixteen rows, and an uncorrelated `EXISTS` never
is. A correlated subquery is always decorrelated, because the alternative is
that it does not run.

That is the first rewrite in the engine that is applied or not *depending on the
data*, which is what having a cost model is for.

## Statistics and cost

```
qe> .stats on
qe> SELECT p.city, COUNT(*) FROM people p JOIN orders o ON p.id = o.person_id
      WHERE p.age > 40 GROUP BY p.city;
```

```text
Project         rows=4  time=0.001ms   est=6  q-error=1.6x
  -> HashAggregate  rows=4  time=0.028ms   est=6  q-error=1.6x
    -> HashJoin     rows=6  time=0.150ms   est=10 q-error=1.6x
      -> Scan       rows=12 time=0.001ms   est=12 q-error=1.0x   table=orders, 6 columns pruned
      -> Filter     rows=7  time=0.076ms   est=6  q-error=1.1x   (age > 40)
        -> Scan     rows=10 time=0.005ms   est=10 q-error=1.0x   table=people, 6 columns pruned,
                                                                 zone maps on (age > 40)
```

Note which side of the join is which: `orders` probes and the *filtered* people
build the hash table, even though the query names people first. That is the cost
model choosing the smaller build side.

### What is collected

Once, when a table enters the catalog, so a plan is never costed against
statistics that might not exist:

| | how | why that one |
| --- | --- | --- |
| distinct count | HyperLogLog, `P = 12` | 4 KiB per column and ~1.6% error whether the column has a thousand distinct values or a billion |
| histogram | equi-depth, 100 buckets over a 20K sample | equi-*width* is useless on skew: if 99% of rows sit in one bucket every range estimate touching it is a guess |
| most common values | top 24 by frequency | a histogram cannot answer equality -- a bucket boundary says nothing about how often one value occurs |
| min / max / null count | exact, from the row-group zone maps | already computed by the write that produced them |

The histogram and the MCV list answer different questions and neither subsumes
the other. Together they cover `x > 5` and
`x = 'the one value that is 40% of the table'`.

### Where the estimates go wrong

Conjunctions are multiplied as if the predicates were independent.
`city = 'London' AND country = 'UK'` is one predicate wearing two hats, and
multiplying underestimates by however correlated they are. This is the single
largest source of error in every optimizer that does it this way, and every
optimizer does it this way.

Join cardinality uses the containment assumption -- `|R| * |S| / max(distinct_R,
distinct_S)` -- which is optimistic whenever the smaller side's values are not
all present in the larger. Errors compound multiplicatively through a join tree:
2x per join is 32x by the fifth.

### Cardinality estimation, scored

`cargo test -p engine --test qerror -- --nocapture` runs every corpus query and
compares each operator's prediction to reality:

```text
cardinality estimation, 3887 operators over the corpus
  median 1.00x   p90 2.29x   p99 10.00x   max 28.78x
  operator               n    median       p90
  Distinct              54     1.43x     4.65x
  Filter               838     1.00x     2.67x
  HashAggregate        218     1.00x     1.54x
  HashJoin             173     1.17x     2.50x
  IndexScan            102     1.00x     1.00x
  Limit                 30     1.00x     1.00x
  NestedLoopJoin         5     1.00x     7.33x
  OneRow                23     1.00x     1.00x
  Project             1064     1.00x     2.50x
  Scan                1024     1.00x     1.00x
  SetOp                 83     1.25x     2.33x
  Sort                 163     1.00x     2.62x
  TopN                   9     3.33x     5.83x
  Window               101     1.00x     2.50x
```

Read that with the fixtures in mind: these tables are ten and twelve rows, so a
histogram over them is nearly exact and the numbers flatter the estimator. The
useful part is the *shape* -- scans exact, filters good, joins worse, and the
worst offenders printed by name so the next improvement is obvious.

A scan's estimate is the catalog's row count, which is an upper bound rather
than a prediction: zone-map pruning and an early-terminating LIMIT both make it
emit fewer. The test asserts the bound rather than exactness, because counting
pruning as an estimation error would reward turning pruning off.

An `IndexScan` is the one operator that is exact by construction, and its
1.00x column is a statement about the design rather than about the estimator.
The lookup has already walked the leaves by the time the operator is built, so
it reports the row count it will actually emit -- the same number the planner
used to choose it over a scan. `TopN` is the opposite extreme at 3.33x, because
its estimate is `min(limit, input)` and the input estimate is the one being
tested.

### The cost model

Every constant was picked by hand and tuned until plans looked right on this
engine's benchmarks. They are not measured, not portable, and not claimed to be
either -- a cost model only ever compares two plans, so what it needs is for the
*ratios* to be roughly true. Treat one unit as "touching one row in a scan".

What matters and is roughly right: building a hash table costs about four times
more per row than probing one, so the smaller input belongs on the build side;
and a nested loop pays per *pair* while a hash join pays per row, which is the
whole reason to hash.

One constant is worth singling out. `compare_pair` is *higher* than a hash
probe, which looks wrong until you look at the operator: this engine's nested
loop materializes every candidate pair into a batch rather than running a tight
comparison loop. The benchmark measures a 1M x 5 nested-loop join at roughly
2.6x the hash join, and the constant is what reproduces that ratio. A cost model
that describes a nested loop somebody else wrote is worse than useless.

### Join ordering

DPsize over subsets: for each subset of the relations keep the cheapest way to
join exactly those, building subsets in increasing size so every split has both
halves already solved. `3^n` splits, which is fine to ten relations and hopeless
past fifteen -- hence a cap, beyond which a greedy heuristic takes over.

Both orientations of every split are enumerated, so the DP also picks which side
to build. That falls out of the cost model rather than being a special case.

Only inner and cross joins are reordered. An outer join is not freely
associative -- moving it changes which rows get NULL-padded -- so it is an
opaque atom the reorderable joins are arranged around.

Termination is by cost: a reordering is only accepted when it is strictly
cheaper, so the rule cannot cycle with itself or with any other.

The plan it picks is not always the obvious one. Given a fact table and two
small dimensions with no condition between them, it joins the two dimensions
into a 350-row cross product and probes that once with the facts, rather than
probing the facts twice -- because emitting rows dominates and the bushy plan
emits 2000 instead of 4000. That is a real technique, and it is the kind of
answer a cost model gives that a heuristic would not.

## What steps 11-12 bought

`--bench-baseline no-reorder` runs the same queries with every rule *except*
join ordering, so the column is reordering alone:

| query, as written | reordered | as written | speedup |
| --- | ---: | ---: | ---: |
| `boroughs JOIN trips` (dimension first) | 133 ms | 228 ms | 1.7x |
| `vendors JOIN trips JOIN boroughs` | 246 ms | 405 ms | 1.6x |
| `trips JOIN boroughs JOIN vendors` (already good) | 248 ms | 272 ms | 1.1x |
| comma join, conditions in WHERE | 979 ms | 1424 ms | 1.5x |
| filtered fact table, 1.4 ms either way | 1.44 ms | 1.16 ms | 0.8x |
| **total** | **1608 ms** | **2330 ms** | **1.4x** |

The 1.1x row matters as much as the 1.7x ones: a query already written in a good
order is left alone rather than churned into a worse one. The 0.8x row is a
sub-2 ms query where the difference is inside measurement noise.

## What steps 8-10 bought

**Zone maps** (`--bench-baseline no-pruning`, 1M rows, 16 row groups):

| predicate | with | without | speedup | rows |
| --- | ---: | ---: | ---: | ---: |
| `trip_id > 990000` (clustered) | 0.04 ms | 0.37 ms | 9.5x | 9,999 |
| `trip_id BETWEEN 400000 AND 410000` | 0.08 ms | 0.73 ms | 9.7x | 10,001 |
| `trip_id = 777777` | 0.04 ms | 0.35 ms | 8.8x | 1 |
| `fare > 300` (outside the global range) | 0.02 ms | 0.55 ms | 32.7x | 0 |
| `trip_id > 100000` (matches most rows) | 0.87 ms | 0.89 ms | 1.0x | 899,999 |
| `fare > 20` (unclustered, in range) | 0.90 ms | 0.87 ms | 1.0x | 58,819 |
| `pickup_borough = 'Bronx'` | 11.1 ms | 11.1 ms | 1.0x | 200,095 |

The honest half matters as much as the good half. Zone maps do nothing for an
unclustered column whose every row group spans the predicate -- and cost a few
comparisons for the privilege. `trip_id` is generated sequentially, so its
groups hold narrow disjoint ranges; `fare` and `pickup_borough` are scattered,
so every group's range covers the query. Clustering is what makes this index
work, which is why real systems care so much about sort order on load.

**The optimizer** (`--bench-baseline unoptimized`, the join benchmark):

| query | optimized | unoptimized | speedup |
| --- | ---: | ---: | ---: |
| join + filter, 1354 rows out | 1.27 ms | 168.5 ms | **132.7x** |
| join feeding a GROUP BY | 235 ms | 266 ms | 1.1x |
| LEFT join, `COUNT(*)` | 133 ms | 177 ms | 1.3x |
| join with a residual | 142 ms | 201 ms | 1.4x |
| aggregates with no join | -- | -- | 1.0x |

The 130x is predicate pushdown doing the thing predicate pushdown is for. The
step-7 benchmark built a million joined rows so a filter above could keep 1,354;
the filter now sits below the join, and the join builds 1,354. The other join
queries gain only what projection pushdown gives them, because their predicates
are already on the far side of the aggregate or absent entirely.

## Vectorization, measured

```bash
python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
cargo run --release --bin qe -- --load trips=/tmp/trips.csv --bench benches/taxi.sql
```

1M rows, 9 columns, 16 row groups, release build, minimum of 9 runs:

| query | vectorized | scalar | speedup | rows out |
| --- | ---: | ---: | ---: | ---: |
| `distance_mi > 25` | 0.60 ms | 15.37 ms | 25.6x | 531 |
| `distance_mi > 0.5` | 3.42 ms | 23.84 ms | 7.0x | 945,573 |
| `pickup_borough = 'Bronx'` | 11.19 ms | 66.97 ms | 6.0x | 200,095 |
| `pickup_borough = 'Bronx' AND distance_mi > 12` | 11.72 ms | 73.46 ms | 6.3x | 1,788 |
| `distance_mi > 60` | 0.27 ms | 6.98 ms | 26.2x | 7 |
| `fare * 1.2, fare - tip … passengers > 4` | 5.30 ms | 33.44 ms | 6.3x | 333,267 |
| `tip IS NULL` | 2.49 ms | 6.94 ms | 2.8x | 20,038 |
| 4-column projection, `fare > 100` | 0.62 ms | 15.39 ms | 24.9x | 109 |
| `pickup_date >= '2024-07-01'` | 2.00 ms | 23.06 ms | 11.5x | 499,561 |
| `pickup_borough IN ('Bronx','Queens')` | 37.03 ms | 94.76 ms | 2.6x | 400,295 |
| **total** | **74.64 ms** | **360.23 ms** | **4.8x** | |

The step-4 milestone query, `SELECT trip_id FROM trips WHERE distance_mi > 25`,
took **42 ms** before this work and takes **0.60 ms** now. That splits roughly
in two: zero-copy batches took it from 42 ms to 17 ms (the scan had been
copying every slice out of the row group), and vectorized evaluation took it
the rest of the way. The 42 ms and 17 ms are historical -- the intermediate
builds no longer exist to re-time -- so only the final figure comes from the
current sitting.

The weak case is the `IN` list at 2.6x -- two full string-equality passes plus a
400K-row gather. Dictionary encoding, where the comparison happens once against
the dictionary and then over `u32` codes, is the answer, and it is what the
storage layer's encoding work is for.

### What is actually vectorized

Being precise about this, because "SIMD" is easy to claim and hard to verify:

- **Kernels with typed fast paths**: comparisons on Int32, Int64, Float64,
  Date32, Timestamp, Boolean and Utf8, against a constant or another column;
  `AND`/`OR`/`NOT` and `IS NULL` as word-at-a-time bitmap operations; Float64
  arithmetic. Results are packed 64 comparisons to a `u64`.
- **Batch-at-a-time but not SIMD**: integer arithmetic uses checked operations,
  which LLVM will not vectorize. That is deliberate -- silently wrapping would
  be wrong, and arithmetic is far colder than the comparisons that dominate a
  filter. `CAST`, `LIKE` and `CASE` likewise loop per row, but over an already
  resolved tree with typed columns rather than through `ScalarValue`.
- **No hand-written intrinsics.** The loops are shaped for auto-vectorization
  (contiguous slices, no branches, no bounds checks) and `.cargo/config.toml`
  enables `+simd128` for wasm32. Whether v128 instructions are actually emitted
  is unverified: it needs `wasm-objdump` on a linked `.wasm`, which does not
  exist until the wasm crate does. Treat the wasm numbers as unmeasured.

### Joins and aggregates, measured

```bash
cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
    --load boroughs=data/boroughs.csv \
    --bench benches/joins.sql --bench-baseline nested-loop
```

A star-schema join: five-row dimension table built, a million rows streamed
past it. `--bench-baseline nested-loop` forces every join onto the O(n*m) path,
so the speedup column is what hashing bought.

| query | hash join | nested loop | speedup |
| --- | ---: | ---: | ---: |
| join + filter, 1354 rows out | 1.28 ms | 1.85 ms | 1.5x |
| join feeding a GROUP BY | 233 ms | 383 ms | 1.6x |
| LEFT join, every row preserved | 133 ms | 240 ms | 1.8x |
| join with a residual inequality | 141 ms | 230 ms | 1.6x |

Aggregate-only queries appear in the same file at 1.0x, since there is no join
in them to choose an algorithm for. Their absolute times: a global aggregate
over 1M rows is 35 ms, five-group `GROUP BY` with `AVG` is 123 ms, and
`COUNT(DISTINCT)` per group is 147 ms.

**The first row used to be the headline, and predicate pushdown took it away.**
When this table was first measured -- before the optimizer existed -- the filter
sat *above* the join, so a million rows were joined to produce 1,354 and hashing
saved 2.6x. Pushdown now puts the filter under the join, which builds 1,354
rows; at that size the nested loop is fine too, and hashing is worth 1.5x on a
1.3 ms query. The win did not evaporate, it moved: the same query against the
unoptimized plan is 132x faster, and that is the number in the optimizer table
above.

That is the general shape of these ratios. Each measures one thing turned off,
so a later step that removes the work can shrink an earlier step's headline
without either measurement having been wrong.

**Probe keys still allocate.** A text join key builds a `String` per probe row.
Removing that means keying the hash table on a hash and verifying candidates
against the stored build keys. Two cheaper fixes already landed while
benchmarking -- gathering output rows through typed buffers instead of
`ScalarValue` (292 ms -> 178 ms) and reusing the probe key buffer
(178 ms -> 154 ms) -- and the remaining allocation is not worth removing until
pushdown has changed what the hot path even is.

## The compaction heuristic, and why it is off

`Selection::should_compact` decides when a sparse selection should be
materialized into a dense batch. Measured on the fixture above, total time still
rises monotonically with the threshold -- but by far less than it once did:

| threshold | 0.0 (off) | 0.1 | 0.2 | 0.4 |
| --- | ---: | ---: | ---: | ---: |
| total | 74.9 ms | 75.2 ms | 76.2 ms | 78.0 ms |

So it ships off, though the margin is now thin. When this was first measured the
spread was 88 ms to 122 ms -- a 39% penalty at 0.4. It is 4% today, and the
reason is projection pushdown: the filter used to compact all eight columns of
the row group and now compacts the one or two the query actually reads. The
conclusion is unchanged and the direction is unchanged, but a plan shape that
benefits would tip this constant much more easily than the original numbers
suggested.

The reason it loses at all is the shape of today's plans: the only operator
reading through a filter's selection is the projection above it, and a
projection materializes anyway -- so compacting first is a second copy, made
worse because the filter compacts every column while the projection usually
needs one or two. Compaction earns its keep once a sparse selection is read by
several operators in a row (filter over filter, a join probe, a hash aggregate).
The mechanism is implemented, tested and measured; it just has nothing to pay
for yet.

A first pass at this got the wrong answer for a boring reason worth recording:
the sparsity test divided by the *column* length rather than the batch's own
window, so a batch that was 94% full looked sparse against its 65,536-row row
group and got compacted every time. That alone cost 2x.

## Deliberate gaps

- **Nothing records that a table arrived sorted.** The ordering property tracks
  what a `Sort` produces, but a scan never claims one -- so a merge join over a
  clustered column, which should need no sort at all, is not recognised.
- **No window function in an aggregate query.** A window runs after aggregation,
  so its arguments would have to resolve against the aggregate's output; getting
  that wrong gives a wrong answer rather than an error, so it is refused by name.
- **`RANGE` frames only at peer boundaries.** A numeric `RANGE` offset is
  measured in the ORDER BY column's own units and needs per-type arithmetic on
  the bound. `ROWS` offsets work; `GROUPS` frames are rejected.
- **`DISTINCT` and set operations allocate per row.** A row's identity is built
  as a tuple of values, which means a `String` per text column. Hashing the row
  instead is the fix.
- **Joins spill nothing.** The build side is materialized whole, in memory.
- **Statistics are never refreshed.** They are computed when a table is
  registered and never again. Nothing mutates a table yet, so nothing can make
  them stale -- but an `ANALYZE` is the obvious next thing the moment that
  changes.
- **No multi-column statistics.** Correlated predicates are the largest source
  of estimation error and nothing here addresses them.
- **A small rule set.** Predicate simplification (`x > 5 AND x > 3` -> `x > 5`,
  contradiction detection), outer-join-to-inner, limit pushdown and common
  subexpression elimination are still to come. The driver and the trace are
  built for them; each is a new file in `optimizer/rules/`.
- **Correlated subqueries only in WHERE.** A correlated subquery in a
  projection, under an `OR`, or over a plan with `GROUP BY` or `LIMIT` is
  rejected at plan time rather than executed per row. Supporting those needs
  either a mark-join or a per-row execution path, and the error says which shape
  it could not take.
- **Zone maps only see through bare column comparisons.** An implicit cast
  around a column makes a predicate opaque to pruning, because the bounds would
  have to be cast too. Index range extraction and the bloom probes have the same
  blind spot, for the same reason.
- **Parquet is read, never written.** Nothing in the engine produces the
  format, so the fixtures come from pyarrow.
- **No nested Parquet.** Lists, maps and structs need repetition levels and an
  assembly step; they are refused by name rather than silently flattened.
- **Only UNCOMPRESSED and SNAPPY.** GZIP needs an inflate implementation and
  ZSTD considerably more; both are named in the error rather than attempted, as
  are encrypted footers and multi-file datasets.
- **Parquet's own bloom filters are ignored.** A lazily loaded column cannot
  have a filter built for it -- that would mean reading the data it exists to
  avoid reading -- so Parquet tables get zone maps but no bloom pruning. The
  format stores split-block filters of its own, and reading those is the fix.
- **Nanosecond timestamps are truncated to microseconds**, this engine's only
  resolution. Time zones are ignored: a timestamp is a naive instant.
- **Statistics for a lazily loaded table sample one row group.** Bounds and null
  counts are exact over every row group; histograms and distinct counts are
  projected from the first. An explicit `ANALYZE` would be the honest way to ask
  for better.
- **`COUNT(*)` reads a column.** A scan with no predicate and no projection
  could answer it from the catalog's row count; sql.js beats this engine on
  exactly that query because SQLite does.
- **A CTE is opaque to the optimizer.** Its definition is shared by every
  reference, so no rule may rewrite through one of them -- which means
  `WITH t AS (SELECT * FROM big) SELECT * FROM t WHERE x > 5` cannot push the
  filter into the CTE. Inlining a definition referenced exactly once is the
  standard fix and is not done. Postgres shipped the same trade for twenty
  years before adding that heuristic.
- **A CTE is materialized when the plan is built**, not on first read, so it is
  computed even when a `LIMIT` above it would have consumed nothing.
- **No recursive CTEs.** `WITH RECURSIVE` is a fixpoint computation rather than
  a named subquery and is refused by name.
- **Indexes are built eagerly and never maintained.** `.index` walks the column
  and constructs the tree by repeated insertion, which is what exercises the
  split logic but is several times slower than bulk-loading from sorted input.
  Nothing mutates a table, so nothing can invalidate a tree -- deletion is not
  implemented, and a B+ tree without it is only half a B+ tree.
- **One column per index, and no index-only scans.** A composite index would
  need a compound key and a notion of prefix matching. An index-only scan --
  answering `SELECT id ... WHERE id > 5` from the tree alone, without touching
  the column at all -- is a natural next step that nothing here does.
- **`<>` and `OR` are never served from an index.** Both need a union of
  ranges; the extraction carries a single interval, and the union of "everything
  below" and "everything above" is never selective enough to be worth it.
- **Decimal arithmetic degrades to Float64.** Decimals compare and cast exactly
  (rescaling through `i128`), but mixed-type arithmetic goes through `f64`
  rather than faking exact fixed-point results.
- **No `insta`.** Parser snapshot tests compare pretty-printed ASTs against
  inline expectations, which keeps `engine` dependency-free. Same shape of
  assertion; swap in `insta` whenever the dependency is worth it.
- **No result hashing in the sqllogictest harness.** The
  `N values hashing to <md5>` form exists to keep corpus files small; supporting
  it means an MD5 dependency or hand-rolling MD5, neither of which pays yet.

## Testing

`cargo test` -- 308 tests plus a 997-record sqllogictest corpus, every query of
which is additionally run seven ways and compared, run a second time against
Parquet-backed tables, and scored for estimation accuracy.

Unit tests live beside each module; end-to-end tests are in
`crates/engine/src/tests.rs`, with a dedicated NULL-semantics section covering
three-valued comparison, `NOT IN` with a NULL in the list, `AND`/`OR` truth
tables, `CASE` with unknown conditions, and NULL propagation through arithmetic
and concatenation.

### Differential testing against SQLite

`tests/sqllogictest/` holds a corpus in SQLite's sqllogictest format. The
expected results in it were produced by **SQLite**, not by hand, so a failure
means this engine and a known-correct implementation disagree about what a query
means.

```bash
cargo test -p engine --test slt          # run the corpus
qe --slt tests/sqllogictest/null.slt     # run one file, with a diff

python3 tools/gen_expected.py --write tests/sqllogictest/*.slt   # refresh expectations
python3 tools/gen_expected.py --check tests/sqllogictest/*.slt   # verify them in CI
```

`select.slt`, `null.slt`, `expr.slt`, `limit.slt`, `joins.slt`,
`aggregates.slt`, `subqueries.slt`, `ordering.slt`, `setops.slt` and
`windows.slt` are hand-written. `errors.slt` pins diagnostics.
`divergences.slt` records the places this engine deliberately disagrees with
SQLite. `generated.slt` is produced by:

```bash
cd tools && python3 fuzz_queries.py --seed 1 --count 400 > ../tests/sqllogictest/generated.slt
python3 tools/gen_expected.py --write tests/sqllogictest/generated.slt
```

The fuzzer walks the fixture schemas and emits type-directed queries -- WHERE
always gets a boolean, CASE branches always unify, literals are sampled from
values that actually occur so predicates select something. It also generates
joins of all five kinds (with residual conditions on the ON clause, not just in
WHERE), `GROUP BY` / `HAVING` queries, derived tables, and subqueries --
`EXISTS`, `IN`, scalar aggregates, correlated and not -- plus `DISTINCT`,
`ORDER BY`, set operations and window functions. Six hundred queries per seed,
clean across every seed tried, of which 80% return rows.

It knows what not to generate, and each exclusion is a documented divergence
rather than a bug swept aside: correlated `NOT IN` (only an anti-join when
nothing is nullable), correlated subqueries under an `OR` or in a projection
(nothing to turn them into), multi-row scalar subqueries (SQLite takes the first
row, this engine raises), and `SUM` over a boolean.

Ordering needs the same care, and getting it wrong produced two false alarms
worth recording. A window's `ORDER BY` has to be a *total* order before
`ROW_NUMBER`, `LAG` or `LEAD` mean anything -- with ties, which row is
"previous" is up to the engine, and the two disagreed for a perfectly good
reason. And with two different `OVER` clauses in one query the output order is
whichever window sorted last, which SQL does not define either. The generator now
orders by every orderable column, so tied rows are identical rows.

It found two real bugs on its first run, both of which are now fixed and
regression-tested: 32-bit integer arithmetic overflowing on ordinary data, and
implicit casts leaking into user-visible column names.

The harness itself lives in `crates/engine/src/sqllogictest/` rather than in a
test file, because it does no I/O -- it takes the file text and a resolver for
`load` paths. That keeps it usable from `cargo test`, from the CLI, and
eventually from the browser, where the same corpus can run in wasm.

Two conventions make the comparison meaningful across two very different type
systems, since SQLite has no BOOLEAN, DATE or DECIMAL type: booleans render as
`1`/`0`, and dates render as ISO text (whose lexicographic order matches date
order, so comparisons agree). The generator also sets
`PRAGMA case_sensitive_like = ON`, without which every `LIKE` test would
disagree for the wrong reason.

### Where this engine deliberately differs from SQLite

All of these are in `tests/sqllogictest/divergences.slt` and `errors.slt`, and
the fuzzer is taught to avoid generating them. The theme is that SQLite is
dynamically typed and this engine is not.

| | SQLite | here |
| --- | --- | --- |
| `CASE WHEN .. THEN 1.5 ELSE 1 END` | returns whichever branch fired | one unified type, REAL |
| `1 / 0` | `NULL` | error |
| `CAST('abc' AS INT)` | `0` | error |
| `2 IN (1, '2')` | false (storage classes differ) | true (literal folded to the column's type) |
| `CAST(0.1 + 0.2 AS TEXT)` | `0.3` (15 sig. digits) | `0.30000000000000004` (shortest round-trip, as PostgreSQL 12+) |
| integer overflow | promotes to float | error |
| `SUM(bool_col)` | sums the underlying 0/1 | error, SUM is numeric |
| non-grouped column in SELECT | picks an arbitrary row | error, as the standard requires |

### Every fast path needs a slow one to agree with

`tests/evaluator_agreement.rs` runs all 938 corpus queries seven ways and
compares results value for value, data types included -- and demands that when
one configuration raises, the others raise the same way:

| configuration | what it checks |
| --- | --- |
| default | the baseline |
| `ExecOptions::scalar()` | vectorized kernels against the row-at-a-time reference |
| `ExecOptions::nested_loop_joins()` | the hash join against comparing every pair |
| `ExecOptions::unoptimized()` | **every rewrite is semantics-preserving** |
| `ExecOptions::without_pruning()` | a skipped row group truly had nothing in it |
| `ExecOptions::without_bloom_filters()` | the same, for the other pruning structure |
| `ExecOptions::without_index_scans()` | a completely different set of rows, read a completely different way, giving the same answer |

The generated corpus is itself indexed -- `fuzz_queries.py` emits `index`
directives for five columns, including a nullable one -- so several hundred
random queries are planned as index scans and checked against SQLite, then
checked again against themselves with indexes turned off.

`tests/parquet_corpus.rs` then runs the whole corpus a third time with every
table loaded from Parquet instead of CSV. Same queries, same expectations, a
completely different loader underneath.

The nested loop compares every pair against the whole condition, so it is the
obvious implementation the hash join has to agree with -- the same relationship
the scalar evaluator has with the vectorized one, and it covers the queries
where the engine and SQLite happen to agree on returning nothing.

Requiring matching *errors* is what keeps `AND`'s short-circuit behaviour
consistent between a per-row and a per-batch implementation, and it is how the
guarded division bug was caught.

The optimizer row is the one the build order asks for by name: running every
query with the optimizer disabled and demanding identical results is the whole
definition of a rewrite being semantics-preserving, and it holds for all 683
corpus queries including the several hundred generated ones.

Each rule additionally has before/after plan assertions in
`src/optimizer/tests.rs`, run through an optimizer built from that rule alone so
the others do not tidy up after it. Those cover the cases the corpus cannot
express: that a predicate does *not* move into the null-supplying side of an
outer join, that a HAVING condition on an aggregate stays put, that the trace's
steps chain end-to-end, and that the rule set reaches a fixpoint.

The hash aggregate has no such counterpart yet -- a sort-based aggregate is the
other implementation to compare it against, and that needs a Sort operator.

Results must also not depend on execution parameters, so the suite runs a query
across batch sizes of 1, 7, 64, 2048 and 100,000 and across compaction
thresholds and demands identical output.

### Not yet

Property tests (proptest) generating random *plans* and asserting the optimizer
preserves semantics -- that becomes possible once there is an optimizer to
disable. The query fuzzer above is the value-level half of the same idea, and it
already covers the expression semantics that the plan-level tests would rely on.

`cargo check -p engine --target wasm32-unknown-unknown` passes: the engine crate
has no dependencies and no host assumptions. Wall-clock timing reports zero on
wasm until the boundary crate injects a clock.

## What is left

The twenty steps are done. What is not done is written down where it belongs --
in **Deliberate gaps** above, and beside the code that would change. The
shortest list of things that would matter most next:

- **A worker.** The engine runs on the main thread, so a long query freezes the
  tab. Everything is already shaped for it: the clock reaches `performance`
  through the global rather than through `window` precisely so it works there.
- **`COUNT(*)` from the catalog.** sql.js beats this engine on exactly that
  query, and the fix is a scan with no predicate and no projection returning a
  number it already has.
- **Hash the row, not its values.** `DISTINCT`, set operations and join probe
  keys all build a tuple of `ScalarValue`s and allocate a `String` per text
  column. It is the same trap three operators fell into in turn, and it is the
  clearest remaining performance work.
- **Parquet's own bloom filters**, so a lazily loaded column can be pruned on
  equality without reading the data that pruning exists to avoid.
