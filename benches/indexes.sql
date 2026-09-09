-- B+ tree index scans, measured against the same queries with the index
-- ignored.
--
--   python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --index trips.fare --bench benches/indexes.sql --bench-baseline no-index
--
-- `fare` is the interesting column: it is *unclustered*, so every row group's
-- min/max spans nearly the whole domain and zone maps rule out nothing. That is
-- exactly the case an index exists for.
--
-- The queries below are a selectivity ladder, from one row in a thousand up to
-- most of the table. The index should win at the top, converge in the middle,
-- and get out of the way at the bottom -- the planner caps an index scan at 5%
-- of the table, so the last few queries must land within noise of the baseline.
-- An index that only ever helps is an index that was never really chosen.

-- 963 rows, 0.1%. The tree names them; the scan reads a million floats to find
-- them.
SELECT trip_id, vendor, fare FROM trips WHERE fare = 12.51;

-- 3675 rows, 0.4%.
SELECT trip_id, fare, tip FROM trips WHERE fare > 45;

-- The extreme tail, a few dozen rows.
SELECT trip_id, fare FROM trips WHERE fare > 120;

-- Two conjuncts on the indexed column: the tighter bound has to win, or this
-- would blow the cap and fall back.
SELECT trip_id FROM trips WHERE fare > 30 AND fare > 100;

-- 16509 rows, 1.7%. Still under the cap, but now the gather is doing real work.
SELECT COUNT(*), AVG(tip) FROM trips WHERE fare > 30;

-- An index range feeding a grouping aggregate.
SELECT vendor, COUNT(*), AVG(fare) FROM trips WHERE fare > 45 GROUP BY vendor;

-- 34810 rows, 3.5%: just inside the cap, and close enough to the crossover that
-- the two configurations should be near each other. This is the honest edge of
-- the decision.
SELECT COUNT(*) FROM trips WHERE fare BETWEEN 12 AND 13;

-- 58819 rows, 5.9%: just *over* the cap. The tree walks its leaves, gives up
-- part way, and the scan runs -- so this measures what a declined index costs.
SELECT COUNT(*) FROM trips WHERE fare > 20;

-- 87% of the table. Nowhere near indexable.
SELECT COUNT(*) FROM trips WHERE fare < 15;

-- Not an indexable predicate at all: `<>` would need two ranges.
SELECT COUNT(*) FROM trips WHERE fare <> 12.51;

-- Indexed table, but the predicate is on a different column.
SELECT COUNT(*) FROM trips WHERE vendor = 'yellow';
