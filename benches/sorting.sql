-- Sorting, DISTINCT and window functions.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --bench benches/sorting.sql --bench-baseline no-top-n
--
-- The baseline forces a full sort where the engine would use a bounded heap,
-- so the speedup column is what top-N bought. Queries with no LIMIT appear at
-- 1.0x: there is nothing to choose between there, and their absolute times are
-- the interesting part.

-- Top-10 of a million rows: the heap holds ten, the sort would hold a million.
SELECT trip_id, fare FROM trips ORDER BY fare DESC LIMIT 10;

-- Sorting on a column that is not selected, so the key becomes a hidden column
-- and a projection sits between the limit and the sort -- which is what limit
-- pushdown exists to get out of the way.
SELECT trip_id FROM trips ORDER BY fare DESC LIMIT 10;

-- A larger limit still beats a full sort, by less.
SELECT trip_id, fare FROM trips ORDER BY fare DESC LIMIT 5000;

-- Multi-key ordering with an offset: the heap has to keep skip + fetch rows.
SELECT trip_id, vendor, fare FROM trips ORDER BY vendor, fare DESC LIMIT 20 OFFSET 100;

-- No limit, so both sides do the same full sort.
SELECT trip_id FROM trips WHERE distance_mi > 20 ORDER BY fare;

-- DISTINCT over a low-cardinality column: memory is bounded by the number of
-- distinct rows, not by the input.
SELECT DISTINCT pickup_borough, vendor FROM trips;

-- DISTINCT over a high-cardinality column, where that bound is the whole table.
SELECT DISTINCT pickup_date FROM trips;

-- A window function: one sort, then one pass per function.
SELECT vendor, ROW_NUMBER() OVER (PARTITION BY vendor ORDER BY fare) FROM trips WHERE distance_mi > 20;

-- A running total under the default RANGE frame, which has to find peer groups.
SELECT pickup_borough, SUM(fare) OVER (PARTITION BY pickup_borough ORDER BY passengers) FROM trips WHERE distance_mi > 20;

-- UNION has to identify every row; UNION ALL does not.
SELECT COUNT(*) FROM (SELECT vendor FROM trips WHERE fare > 100 UNION SELECT vendor FROM trips WHERE fare < 5) AS x;
