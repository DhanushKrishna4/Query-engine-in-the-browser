-- Benchmark set for the synthetic taxi fixture.
--
--   python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv --bench benches/taxi.sql
--
-- Each query is chosen to isolate one thing.

-- Selective float comparison: the vectorized comparison kernel, almost nothing else.
SELECT trip_id FROM trips WHERE distance_mi > 25;

-- Unselective float comparison: most rows survive, so the filter's output
-- selection stays contiguous and nothing is compacted.
SELECT trip_id FROM trips WHERE distance_mi > 0.5;

-- String equality: no SIMD kernel, but still one pass per batch instead of one
-- ScalarValue per row.
SELECT trip_id FROM trips WHERE pickup_borough = 'Bronx';

-- Conjunction: the second predicate only runs on rows the first left undecided.
SELECT trip_id FROM trips WHERE pickup_borough = 'Bronx' AND distance_mi > 12;

-- Very selective: few rows survive a wide batch, so the filter compacts.
SELECT trip_id, fare FROM trips WHERE distance_mi > 60;

-- Projection-heavy: arithmetic over every surviving row.
SELECT trip_id, fare * 1.2, fare - tip FROM trips WHERE passengers > 4;

-- NULL handling on a real column.
SELECT trip_id FROM trips WHERE tip IS NULL;

-- A wide projection over an unfiltered scan: this is where zero-copy batches
-- and Arc-shared columns matter most.
SELECT vendor, pickup_borough, passengers, distance_mi FROM trips WHERE fare > 100;

-- Date comparison against a folded string literal.
SELECT trip_id FROM trips WHERE pickup_date >= '2024-07-01';

-- IN list over a low-cardinality string column.
SELECT trip_id FROM trips WHERE pickup_borough IN ('Bronx', 'Queens');
