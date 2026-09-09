-- The two operator choices step 7 was missing: merge join against hash join,
-- and the streaming aggregate against the hash aggregate.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --load boroughs=data/boroughs.csv \
--       --bench benches/algorithms.sql --bench-baseline merge-join
--
--   ... --bench-baseline stream-aggregate
--
-- The baselines here *force* the alternative -- sorting both join inputs, or
-- sorting the aggregate's input -- because almost nothing in a plan arrives
-- sorted on its own. That makes the comparison sort-merge versus hash, which is
-- the choice a real optimizer weighs, rather than merge versus hash with the
-- sorts pretended away.

-- Joins. The build side is five rows, which is the case a hash join is best at
-- and a sort-merge worst: sorting a million rows to meet a five-row table.
SELECT COUNT(*) FROM trips JOIN boroughs ON trips.pickup_borough = boroughs.borough;

SELECT boroughs.region, COUNT(*) FROM trips JOIN boroughs
  ON trips.pickup_borough = boroughs.borough GROUP BY boroughs.region;

SELECT COUNT(*) FROM trips LEFT JOIN boroughs
  ON trips.pickup_borough = boroughs.borough;

-- Aggregates, from three groups up to a third of a million. The hash table
-- grows with the group count; the streaming aggregate holds one accumulator
-- set whatever the count, and pays for a sort instead.
SELECT vendor, COUNT(*), AVG(fare) FROM trips GROUP BY vendor;

SELECT pickup_borough, vendor, COUNT(*), SUM(fare) FROM trips
  GROUP BY pickup_borough, vendor;

SELECT pickup_date, COUNT(*), SUM(fare) FROM trips GROUP BY pickup_date;

SELECT medallion, COUNT(*) FROM trips GROUP BY medallion;

-- No grouping at all: one group, so there is nothing to sort and nothing to
-- hash. Both configurations run the same operator here.
SELECT COUNT(*), SUM(fare), AVG(distance_mi) FROM trips;
