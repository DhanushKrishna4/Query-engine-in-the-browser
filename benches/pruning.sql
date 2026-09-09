-- Zone-map pruning, measured against the same queries with it turned off.
--
--   cargo run --release --bin qe -- --load trips=/tmp/trips.csv \
--       --bench benches/pruning.sql --bench-baseline no-pruning
--
-- `trip_id` is generated sequentially, so it is *clustered*: each row group's
-- min/max covers a narrow, disjoint range and a predicate on it eliminates
-- whole groups. `fare` is not clustered -- every group's range covers almost
-- the whole domain -- which is exactly the case where zone maps do nothing, and
-- it is in here so the honest half of the story is visible too.

-- Clustered, highly selective: only the last row group can match.
SELECT trip_id, fare FROM trips WHERE trip_id > 990000;

-- Clustered range in the middle of the table.
SELECT trip_id FROM trips WHERE trip_id BETWEEN 400000 AND 410000;

-- Clustered equality: one group.
SELECT trip_id, vendor FROM trips WHERE trip_id = 777777;

-- Clustered, but matching most of the table: nothing to prune.
SELECT trip_id FROM trips WHERE trip_id > 100000;

-- Outside the global range: no fare is anywhere near 300, so every row group is
-- eliminated on metadata alone and the query touches no data at all. Clustering
-- is irrelevant here -- a predicate no value can satisfy prunes everything.
SELECT trip_id FROM trips WHERE fare > 300;

-- Unclustered and in range: every row group's min/max spans the predicate, so
-- nothing can be ruled out and pruning costs a few comparisons for nothing.
SELECT trip_id FROM trips WHERE fare > 20;

-- Unclustered string column, same story.
SELECT trip_id FROM trips WHERE pickup_borough = 'Bronx';
