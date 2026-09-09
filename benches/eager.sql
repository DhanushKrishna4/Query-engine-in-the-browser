-- Aggregate pushdown through a join: a partial aggregate below it, combined
-- above.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --load boroughs=data/boroughs.csv --load vendors=data/vendors.csv \
--       --bench benches/eager.sql --bench-baseline no-eager-aggregate
--
-- The win depends entirely on how far the partial collapses its input before
-- the join sees it, so the queries below run from "collapses a million rows to
-- five" down to "collapses nothing".

-- Five groups out of a million rows, joined to a five-row dimension.
SELECT b.region, COUNT(*), SUM(t.fare) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY b.region;

SELECT b.borough, COUNT(*) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY b.borough;

-- Three groups, and a MIN/MAX pair that combines with itself.
SELECT v.company, MIN(t.fare), MAX(t.fare) FROM trips t
  JOIN vendors v ON t.vendor = v.vendor_code GROUP BY v.company;

-- Grouping by a column from each side.
SELECT b.region, t.vendor, COUNT(*), SUM(t.tip) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY b.region, t.vendor;

-- 336 groups: the partial still collapses, by less.
SELECT b.region, t.pickup_date, COUNT(*) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY b.region, t.pickup_date;

-- AVG is not decomposable, so this one is left alone and both configurations
-- run the same plan.
SELECT b.region, AVG(t.fare) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY b.region;

-- Grouping by a near-unique column collapses nothing; the estimator should
-- refuse the rewrite and these should land on top of each other.
SELECT t.medallion, COUNT(*) FROM trips t
  JOIN boroughs b ON t.pickup_borough = b.borough GROUP BY t.medallion;
