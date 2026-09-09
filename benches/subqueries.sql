-- Subqueries, measured against the same queries with decorrelation off.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --load boroughs=data/boroughs.csv \
--       --bench benches/subqueries.sql --bench-baseline no-decorrelation
--
-- With decorrelation off, an *uncorrelated* IN or EXISTS is still runnable:
-- the subquery is evaluated once and its results become a literal list. That
-- is the honest comparison, and it is what these queries measure. A
-- *correlated* subquery has no such fallback, so those are not in this file --
-- there is no baseline to compare them to, which is itself the point.

-- A small IN list: materializing two values is no worse than a semi-join, and
-- possibly better.
SELECT COUNT(*) FROM trips WHERE pickup_borough IN (SELECT borough FROM boroughs WHERE region = 'Outer');

-- A large IN list: the subquery returns thousands of values, and comparing
-- every row against every one of them is exactly what a hash join is for.
SELECT COUNT(*) FROM trips WHERE trip_id IN (SELECT trip_id FROM trips WHERE distance_mi > 20);

-- Larger still.
SELECT COUNT(*) FROM trips WHERE trip_id IN (SELECT trip_id FROM trips WHERE distance_mi > 12);

-- NOT IN over a non-nullable column, which can become an anti-join.
SELECT COUNT(*) FROM trips WHERE trip_id NOT IN (SELECT trip_id FROM trips WHERE distance_mi > 20);

-- EXISTS with no correlation is a constant either way.
SELECT COUNT(*) FROM trips WHERE EXISTS (SELECT 1 FROM boroughs WHERE congestion_fee > 2);

-- A derived table, unaffected by decorrelation but worth timing.
SELECT COUNT(*) FROM (SELECT trip_id, fare FROM trips WHERE fare > 50) AS x WHERE x.fare < 80;
