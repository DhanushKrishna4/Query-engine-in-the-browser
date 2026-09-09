-- Join and aggregate benchmarks against the synthetic taxi fixture.
--
--   python3 tools/gen_sample.py 1000000 > /tmp/trips.csv
--   cargo run --release --bin qe -- \
--       --load trips=/tmp/trips.csv --load boroughs=data/boroughs.csv \
--       --bench benches/joins.sql --bench-baseline nested-loop
--
-- The `boroughs` dimension is tiny and `trips` is not, which is the shape a
-- hash join is for: build a five-row table, stream a million rows past it.

-- Star-schema join, every row matching.
SELECT trips.trip_id, boroughs.region FROM trips JOIN boroughs ON trips.pickup_borough = boroughs.borough WHERE trips.distance_mi > 20;

-- The same join feeding an aggregate.
SELECT boroughs.region, COUNT(*) FROM trips JOIN boroughs ON trips.pickup_borough = boroughs.borough GROUP BY boroughs.region;

-- Outer join: every probe row survives, matched or not.
SELECT COUNT(*) FROM trips LEFT JOIN boroughs ON trips.pickup_borough = boroughs.borough;

-- Join with a residual condition on top of the equi-key.
SELECT COUNT(*) FROM trips JOIN boroughs ON trips.pickup_borough = boroughs.borough AND trips.fare > boroughs.congestion_fee * 10;

-- Global aggregate over the whole table.
SELECT COUNT(*), SUM(fare), AVG(distance_mi), MIN(tip), MAX(tip) FROM trips;

-- Low-cardinality grouping: five groups over a million rows.
SELECT pickup_borough, COUNT(*), AVG(fare) FROM trips GROUP BY pickup_borough;

-- Two grouping keys.
SELECT vendor, pickup_borough, COUNT(*), SUM(tip) FROM trips GROUP BY vendor, pickup_borough;

-- High-cardinality grouping: one group per day.
SELECT pickup_date, COUNT(*), SUM(fare) FROM trips GROUP BY pickup_date;

-- COUNT DISTINCT builds a set per group.
SELECT pickup_borough, COUNT(DISTINCT passengers) FROM trips GROUP BY pickup_borough;

-- Aggregate over a filtered scan.
SELECT vendor, COUNT(*), AVG(tip) FROM trips WHERE distance_mi > 5 GROUP BY vendor HAVING COUNT(*) > 100;
